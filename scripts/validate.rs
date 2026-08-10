#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! validate.rs — Hermit's validation driver.
//!
//! This is the sole validation driver. Every production caller invokes it
//! directly; the former shell implementation has been removed. The repository-
//! root `validate.sh` is an audited reminder alias with no independent behavior.
//!
//! # Contract
//!
//! * **Everything runs as a `safe-ci-dag-runner` node.** Preflight, the manifest
//!   gate, every CI-lane node, and every compatibility probe. The driver makes
//!   exactly one kind of call — `run_dag_boxed_deadline` (unbounded when no
//!   whole-run budget is supplied) — and never spawns a gate itself. See
//!   `lib/validate_plan.rs` for why that rule is load-bearing and for
//!   the measured evidence that an undeclared node is unboxed.
//! * **Boxing is fail-closed.** Default path re-execs into a transient
//!   `systemd --user` scope; if two-level cgroup-v2 boxing cannot be established
//!   the driver exits 3 rather than running unboxed.
//! * **Output is bounded by default.** Verbosity 1 prints O(1) lifecycle lines per
//!   DAG step. Verbosity 2 streams tagged step output, and verbosity 5 additionally
//!   carries the deepest test identity the runner can observe on every streamed line.
//!   Failures always print their complete captured detail at every level.
//! * **Every claim carries its conditions.** One ledger write point emits the
//!   profile, the executed/skipped/failed counts, commit anchoring, the tree hash,
//!   the toolchain, and the absolute durable log path together, so a downstream
//!   reader can never pair a bare `pass` with inferred coverage.
//! * **`HERMIT_DIR` is a USER-facing setting.** Validation never writes there.
//!   Run state goes to `target/validation/`, durable logs to `ignored/validate/`.
//!
//! # CLI
//!
//! The flag surface preserves the former driver's CLI because in-tree callers
//! depend on it — notably
//! `ci/dag/portable.json`'s `test.strict_compat` node, which invokes
//! `./scripts/validate.rs --portable-strict-compat-only`, plus
//! `.github/workflows/validation-levels.yml`, three `Makefile` targets, and
//! `hermit-cli/tests/{analyze,rr_suite}.rs`. Changing the surface would have
//! required touching all of them in the same change.
//!
//! ```cargo
//! [dependencies]
//! safe-ci-dag-runner = { path = "../agent-utils/rs/safe-ci-dag-runner" }
//! serde_json = "1"
//! libc = "0.2"
//! ```

// `serde_json::json!` expands one recursive macro level PER FIELD, and the ledger
// record is one literal carrying every qualification a reader needs. Keeping it a
// single literal is the point — it is what makes "the row states its own
// conditions" checkable by eye — so the limit is raised rather than the record
// split across statements where a field could be added on one path and not the
// other.
#![recursion_limit = "512"]

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

#[path = "lib/validate_corpus.rs"]
mod validate_corpus;

#[path = "lib/validate_envelope.rs"]
mod validate_envelope;

#[path = "lib/validate_history.rs"]
mod validate_history;

#[path = "lib/validate_plan.rs"]
mod validate_plan;

#[path = "lib/validate_receipt.rs"]
mod validate_receipt;

#[path = "lib/validate_runtime.rs"]
mod validate_runtime;

#[path = "lib/validate_super.rs"]
mod validate_super;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::sync::Arc;

use safe_ci_dag_runner::cgroup::install_scope_teardown;
use safe_ci_dag_runner::cgroup::is_in_scope;
use safe_ci_dag_runner::cgroup::attempt_scope_reexec;
use safe_ci_dag_runner::cgroup::expected_scope_runtime_max_s;
use safe_ci_dag_runner::cgroup::verify_scope_runtime_max;
use safe_ci_dag_runner::cgroup::CgroupManager;
use safe_ci_dag_runner::cgroup::Cgroups;
use safe_ci_dag_runner::model::DagConfig;
use safe_ci_dag_runner::model::RunResult;
use safe_ci_dag_runner::model::StepOutcome;
use safe_ci_dag_runner::perflog::append_step_profiles;
use safe_ci_dag_runner::scheduler::run_dag_boxed_deadline;
use safe_ci_dag_runner::scheduler::steps_violating_run_timeout;
use safe_ci_dag_runner::scheduler::BoxedCgroups;
use safe_ci_dag_runner::scheduler::monotonic_now_ns;
use safe_ci_dag_runner::scheduler::STEP_STARTED_MONOTONIC_NS_ENV;

use validate_plan::CompatMode;

/// Current receipt schema. Missing evidence is represented by explicit nulls;
/// a new writer must never downgrade itself into the schema-4 grandfather.
const COVERAGE_LEDGER_SCHEMA_VERSION: i64 = 5;

/// Recorded in each row so a version-aware reader can tell which driver produced
/// it without inference.
const LEDGER_PRODUCER: &str = "hermit-validate-rs";

/// The Reverie-pin preflight node's tag. Named once so the plan that creates it
/// and the fail-closed assertion that requires it cannot drift apart.
const PIN_GATE_TAG: &str = "pre.reverie_pin";

const LEDGER_ENV: &str = "HERMIT_VALIDATE_LEDGER";
const PARENT_ENV: &str = "DEV_HERMIT_PARENT";
const OWN_SCOPE_DEADLINE_ENV: &str = "HERMIT_VALIDATE_SCOPE_DEADLINE_MONOTONIC_NS";

/// Standalone-only in-repo ledger directory.
///
/// Admitted runs never write here: they send their HistoryRow to the parent's
/// canonical adapter. This fallback exists only for a checkout with no
/// dev-hermit parent and is deliberately not a qualifying receipt authority.
const LEDGER_DIR: &str = "ci/validate-ledger";

/// Fleet/team identity component of the shard name. Overridable so a different
/// team's runs land in a different shard rather than interleaving.
const LEDGER_TEAM_ENV: &str = "VALIDATE_LEDGER_TEAM";
const LEDGER_TEAM_DEFAULT: &str = "local";

// --------------------------------------------------------------------------- args

/// Validation level, mirroring `VALIDATION_LEVEL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Quick,
    PortableOnly,
    Full,
    Super,
}

impl Level {
    fn parse(s: &str) -> Option<Level> {
        match s {
            "quick" => Some(Level::Quick),
            "portable-only" => Some(Level::PortableOnly),
            "full" => Some(Level::Full),
            "super" => Some(Level::Super),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Level::Quick => "quick",
            Level::PortableOnly => "portable-only",
            Level::Full => "full",
            Level::Super => "super",
        }
    }
}

/// A focused mode runs exactly one matrix/lane and exits. At most one may be
/// active, and none may combine with an explicit level — the same two-way
/// exclusion `validate.sh` enforces (validate.sh:360-367).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Focused {
    StrictCompat,
    PortableStrictCompat,
    RrCompat,
    SabreCompat,
    E9patchCompat,
    LiteinstCompat,
    QemuL2,
    PrivilegedOnly,
    Only { lane: String, nodes: String },
    Selective { shallow: bool },
    /// `--envelope-only`, plus `--envelope-compare FILE` which is the same
    /// measurement followed by a monotonicity check (validate.sh:172-176).
    Envelope { baseline: Option<PathBuf> },
}

impl Focused {
    /// The `VALIDATION_PROFILE` string recorded in the ledger, matching
    /// validate.sh:381-392 so history for a profile stays continuous.
    fn profile(&self) -> String {
        match self {
            Focused::StrictCompat => "strict-compat-only".into(),
            Focused::PortableStrictCompat => "portable-strict-compat-only".into(),
            Focused::RrCompat => "rr-compat-only".into(),
            Focused::SabreCompat => "sabre-compat-only".into(),
            Focused::E9patchCompat => "e9patch-compat-only".into(),
            Focused::LiteinstCompat => "liteinst-compat-only".into(),
            Focused::QemuL2 => "qemu-l2-only".into(),
            Focused::PrivilegedOnly => "privileged-only".into(),
            Focused::Only { lane, .. } => format!("only-{lane}"),
            Focused::Selective { .. } => "selective".into(),
            // Both spellings record ONE profile, matching validate.sh:382, so
            // envelope history stays continuous whether or not a baseline was
            // supplied.
            Focused::Envelope { .. } => "envelope-only".into(),
        }
    }
    /// `--all/--full-run` refuses to combine with any focused mode; this is the
    /// name used in that refusal message.
    fn cli_name(&self) -> &'static str {
        match self {
            Focused::StrictCompat => "strict-compat-only",
            Focused::PortableStrictCompat => "portable-strict-compat-only",
            Focused::RrCompat => "rr-compat-only",
            Focused::SabreCompat => "sabre-compat-only",
            Focused::E9patchCompat => "e9patch-compat-only",
            Focused::LiteinstCompat => "liteinst-compat-only",
            Focused::QemuL2 => "qemu-l2-only",
            Focused::PrivilegedOnly => "privileged-only",
            Focused::Only { .. } => "only",
            Focused::Selective { shallow } => {
                if *shallow {
                    "shallow-select"
                } else {
                    "selective"
                }
            }
            Focused::Envelope { baseline } => {
                if baseline.is_some() {
                    "envelope-compare"
                } else {
                    "envelope-only"
                }
            }
        }
    }
}

struct Args {
    level: Level,
    level_explicit: bool,
    focused: Option<Focused>,
    force_full: bool,
    baseline: Option<String>,
    run_on_dirty_tree: bool,
    ignore_cache: bool,
    label_pr: bool,
    verbosity: i64,
    jobs: Option<i64>,
    keep_going: bool,
    allow_cgroup_failure: bool,
    /// Wall budget for the whole validate invocation, across lanes and retries.
    run_timeout: Option<i64>,
    merge_lanes: bool,
    reuse_parent_manifest_gate: bool,
    self_test: bool,
    show_plan: bool,
}

fn usage() -> &'static str {
    "Usage: ./scripts/validate.rs [LEVEL] [OPTIONS]\n\
     \n\
     Run Hermit's local validation suite. Every gate executes as a boxed\n\
     safe-ci-dag-runner DAG node; nothing runs outside the runner.\n\
     \n\
     Levels:\n\
     \x20 quick            Core ptrace run/verify/record smoke tests; no alternate backends.\n\
     \x20 portable-only    Portable build, test, lint, format, and doc gates matching\n\
     \x20                  GitHub-managed portable CI; no PMU or namespace requirements.\n\
     \x20 full             quick plus the complete suite and DBI/KVM gates (default).\n\
     \x20 super            Repeat stress probes under moderate oversubscription.\n\
     \x20 --quick          Alias for the quick level.\n\
     \x20 --portable       Alias for the portable-only level.\n\
     \n\
     Focused gates (run one matrix/lane and exit):\n\
     \x20 --strict-compat-only          Run the blocking L2 app matrix.\n\
     \x20 --portable-strict-compat-only Portable L2 matrix with bounded diagnostics.\n\
     \x20 --rr-compat-only              Gate the known-passing record/replay matrix.\n\
     \x20 --sabre-compat-only           Gate the measured SaBRe matrix.\n\
     \x20 --e9patch-compat-only         Gate core + installed e9patch L2 apps.\n\
     \x20 --liteinst-compat-only        Run the portable CI liteinst_strict test.\n\
     \x20 --qemu-l2-only                Run the heavyweight QEMU L2 boot.\n\
     \x20 --portable-only               No PMU/CPUID hardware required.\n\
     \x20 --privileged-only             PMU/CPUID-dependent tests only.\n\
     \x20 --only <lane> <group.job>[,...]  Run ONE DAG shard (no deps).\n\
     \x20 --selective, --since-green    Only nodes affected since the last green baseline.\n\
     \x20 --shallow-select              Like --selective but pin the baseline to HEAD~1.\n\
     \x20 --baseline <sha>              Known-green baseline commit for --selective.\n\
     \x20 --envelope-only               Measure and emit the working-envelope vector (JSON + human).\n\
     \x20 --envelope-compare FILE       Measure, then fail if any count regressed below FILE.\n\
     \x20 --all, --full-run             Assert the COMPLETE suite explicitly.\n\
     \n\
     Other options:\n\
     \x20 --verbose        Verbosity level 2: stream tagged per-step output.\n\
     \x20 --verbosity N    Output level 1..5 (default 1; levels 3/4 currently equal 2;\n\
     \x20                  level 5 prefixes every streamed line with test identity).\n\
     \x20 --run-on-dirty-tree  Escape hatch; AGENTS SHOULD NOT USE THIS.\n\
     \x20 --label-pr       Publish a receipt and label the PR after a full green (default).\n\
     \x20 --no-label-pr    Disable the non-fatal receipt publication and label update.\n\
     \x20 --ignore-cache   Force a real run even on a tree-keyed cache hit.\n\
     \x20 -j N             Scheduler width (default: host_cpus/8, floor 2, cap 16).\n\
     \x20 --run-timeout SEC  Wall budget for the WHOLE invocation (across lanes and\n\
     \x20                  retries). On breach, in-flight nodes are cut and the run still\n\
     \x20                  reports instead of being killed externally. Also sets a later\n\
     \x20                  systemd-scope backstop. Env: HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS.\n\
     \x20 -k, --keep-going Do not eager-exit on the first failure.\n\
     \x20 --allow-cgroup-failure  Downgrade to an UNBOXED run instead of failing closed.\n\
     \x20 --merge-lanes    Fuse the portable and privileged lanes (the full default).\n\
     \x20 --sequential-lanes  Diagnostic fallback: run full lanes back to back.\n\
     \x20 --show-plan      Print the boxed DAG plan (nodes, caps, deps) and exit.\n\
     \x20 --self-test      Run the driver's inert policy/quoting brackets and exit.\n\
     \x20 -h, --help       Show this help and exit.\n\
     \n\
     Environment: VALIDATE_LEVEL, VALIDATE_LABEL_PR, VALIDATE_RUN_ON_DIRTY_TREE,\n\
     VALIDATE_IGNORE_CACHE, VALIDATE_VERBOSITY, VALIDATE_VERBOSE, VALIDATE_FORCE_FULL,\n\
     HERMIT_VALIDATE_LEDGER, PR_NUMBER, SUPER_REPETITIONS, L4_REPS, ENVELOPE_JSON,\n\
     HERMIT_LAST_GREEN_SHA, CI_HUB_APPLY_LOCAL_LABEL, DEV_HERMIT_PARENT."
}

fn env_flag(name: &str, want: &str) -> bool {
    std::env::var(name).map(|v| v == want).unwrap_or(false)
}

fn parse_verbosity(value: &str) -> Result<i64, u8> {
    match value.parse::<i64>() {
        Ok(v @ 1..=5) => Ok(v),
        _ => {
            eprintln!("validate: verbosity must be an integer from 1 through 5, got {value:?}");
            Err(2)
        }
    }
}

fn env_verbosity() -> Result<i64, u8> {
    match std::env::var("VALIDATE_VERBOSITY") {
        Ok(v) if !v.is_empty() => parse_verbosity(&v),
        _ => Ok(if env_flag("VALIDATE_VERBOSE", "1") { 2 } else { 1 }),
    }
}

fn parse_args() -> Result<Args, u8> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    parse_argv(&argv)
}

/// Argument parsing over an EXPLICIT argv.
///
/// Split out from [`parse_args`] so `--self-test` can exercise the real parser
/// on synthetic command lines without spawning a subprocess — a subprocess would
/// re-enter `main`, hit the dirty-tree and rebase-freshness gates, and turn a CLI
/// bracket into a test of the checkout's state instead of the flag surface.
fn parse_argv(argv: &[String]) -> Result<Args, u8> {
    let mut level = Level::Full;
    let mut level_explicit = false;
    if let Ok(v) = std::env::var("VALIDATE_LEVEL") {
        if !v.is_empty() {
            match Level::parse(&v) {
                Some(l) => {
                    level = l;
                    level_explicit = true;
                }
                None => {
                    eprintln!("validate: invalid VALIDATE_LEVEL: {v}");
                    return Err(2);
                }
            }
        }
    }
    let mut focused: Vec<Focused> = Vec::new();
    let verbosity = env_verbosity()?;
    let mut args = Args {
        level,
        level_explicit,
        focused: None,
        force_full: env_flag("VALIDATE_FORCE_FULL", "1"),
        baseline: None,
        run_on_dirty_tree: env_flag("VALIDATE_RUN_ON_DIRTY_TREE", "1"),
        ignore_cache: env_flag("VALIDATE_IGNORE_CACHE", "1"),
        label_pr: !env_flag("VALIDATE_LABEL_PR", "0"),
        verbosity,
        jobs: None,
        keep_going: false,
        allow_cgroup_failure: false,
        run_timeout: None,
        merge_lanes: true,
        reuse_parent_manifest_gate: false,
        self_test: false,
        show_plan: false,
    };
    let mut shallow = false;
    let mut selective = false;
    let mut show_plan = false;
    let mut envelope = false;
    let mut envelope_baseline: Option<PathBuf> = None;

    let mut i = 0;
    let set_level = |args: &mut Args, l: Level| -> Result<(), u8> {
        if args.level_explicit {
            eprintln!("validate: choose only one validation level");
            return Err(2);
        }
        args.level = l;
        args.level_explicit = true;
        Ok(())
    };
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "quick" | "portable-only" | "full" | "super" => {
                set_level(&mut args, Level::parse(a).unwrap())?
            }
            "--quick" => set_level(&mut args, Level::Quick)?,
            "--portable" | "--portable-only" => set_level(&mut args, Level::PortableOnly)?,
            "--strict-compat-only" => focused.push(Focused::StrictCompat),
            "--portable-strict-compat-only" => focused.push(Focused::PortableStrictCompat),
            "--rr-compat-only" => focused.push(Focused::RrCompat),
            "--sabre-compat-only" => focused.push(Focused::SabreCompat),
            "--e9patch-compat-only" => focused.push(Focused::E9patchCompat),
            "--liteinst-compat-only" => focused.push(Focused::LiteinstCompat),
            "--qemu-l2-only" => focused.push(Focused::QemuL2),
            "--privileged-only" => focused.push(Focused::PrivilegedOnly),
            // `--envelope-only` and `--envelope-compare` are ONE mode in
            // validate.sh (both set ENVELOPE_MODE=only; the second merely adds a
            // baseline, validate.sh:172-176), so they accumulate into a single
            // Focused entry rather than colliding as two focused modes.
            "--envelope-only" => envelope = true,
            "--envelope-compare" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => {
                        envelope = true;
                        envelope_baseline = Some(PathBuf::from(v));
                    }
                    _ => {
                        eprintln!("validate: --envelope-compare needs a FILE");
                        return Err(2);
                    }
                }
            }
            "--show-plan" => show_plan = true,
            "--selective" | "--since-green" => selective = true,
            "--shallow-select" => {
                selective = true;
                shallow = true;
            }
            "--all" | "--full-run" => args.force_full = true,
            "--run-on-dirty-tree" => args.run_on_dirty_tree = true,
            "--ignore-cache" => args.ignore_cache = true,
            "--label-pr" => args.label_pr = true,
            "--no-label-pr" => args.label_pr = false,
            "--verbose" => args.verbosity = 2,
            "--verbosity" => {
                i += 1;
                args.verbosity = match argv.get(i) {
                    Some(v) => parse_verbosity(v)?,
                    None => {
                        eprintln!("validate: --verbosity needs a level from 1 through 5");
                        return Err(2);
                    }
                };
            }
            "--merge-lanes" => args.merge_lanes = true,
            "--sequential-lanes" => args.merge_lanes = false,
            // Internal nested-payload optimization. The outer full DAG has
            // already run the exact same manifest command and structurally
            // gates this node on it. The nested payload still reruns submodule
            // and Reverie-pin checks, so `reverie_pin_current` remains observed.
            "--reuse-parent-manifest-gate" => args.reuse_parent_manifest_gate = true,
            "--self-test" => args.self_test = true,
            "-k" | "--keep-going" => args.keep_going = true,
            "--allow-cgroup-failure" => args.allow_cgroup_failure = true,
            "--run-timeout" => {
                i += 1;
                match argv.get(i).and_then(|v| v.parse::<i64>().ok()) {
                    Some(v) if v > 0 => args.run_timeout = Some(v),
                    _ => {
                        eprintln!("validate: --run-timeout needs a positive number of SECONDS");
                        return Err(2);
                    }
                }
            }
            "--baseline" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => args.baseline = Some(v.clone()),
                    _ => {
                        eprintln!("validate: --baseline needs a SHA");
                        return Err(2);
                    }
                }
            }
            "-j" => {
                i += 1;
                match argv.get(i).and_then(|v| v.parse::<i64>().ok()) {
                    Some(n) if n > 0 => args.jobs = Some(n),
                    _ => {
                        eprintln!("validate: -j needs a positive integer");
                        return Err(2);
                    }
                }
            }
            "--only" => {
                let lane = argv.get(i + 1).cloned().unwrap_or_default();
                let nodes = argv.get(i + 2).cloned().unwrap_or_default();
                if lane.is_empty() || nodes.is_empty() {
                    eprintln!("validate: --only needs <lane> <group.job>[,<group.job>...]");
                    eprintln!("          e.g. ./scripts/validate.rs --only portable test.sabre_examples");
                    return Err(2);
                }
                focused.push(Focused::Only { lane, nodes });
                i += 2;
            }
            "-h" | "--help" => {
                println!("{}", usage());
                return Err(0);
            }
            other => {
                eprintln!("validate: unknown argument: {other} (try --help)");
                return Err(2);
            }
        }
        i += 1;
    }
    if selective {
        focused.push(Focused::Selective { shallow });
    }
    if envelope {
        focused.push(Focused::Envelope { baseline: envelope_baseline });
    }
    if focused.len() > 1 {
        eprintln!("validate: choose only one focused validation mode");
        return Err(2);
    }
    if args.level_explicit && !focused.is_empty() {
        eprintln!("validate: validation levels cannot be combined with focused validation modes");
        return Err(2);
    }
    args.show_plan = show_plan;
    args.focused = focused.pop();
    if args.reuse_parent_manifest_gate
        && (!matches!(args.focused, Some(Focused::PortableStrictCompat)) || args.label_pr)
    {
        eprintln!(
            "validate: --reuse-parent-manifest-gate is internal to the no-label \
             portable-strict payload of the full DAG"
        );
        return Err(2);
    }
    // `--privileged-only` and `--portable-only` are spelled as focused flags but
    // one of them is a LEVEL in validate.sh. Preserve that: --portable-only sets
    // the level, --privileged-only stays focused (validate.sh:169,189).
    if !force_full_policy_allows(
        args.force_full,
        args.level,
        args.focused.as_ref().map(|f| f.cli_name()),
    ) {
        eprintln!(
            "validate: --all/--full-run requires level full and forbids every focused or selective mode"
        );
        return Err(2);
    }
    if shallow && args.baseline.is_some() {
        eprintln!("validate: --shallow-select forces a HEAD~1 baseline; do not also pass --baseline");
        return Err(2);
    }
    Ok(args)
}

/// `force_full_policy_allows` (validate.sh:299): `--all` asserts the COMPLETE
/// suite, so it accepts only the unfocused `full` level.
fn force_full_policy_allows(force_full: bool, level: Level, focused: Option<&str>) -> bool {
    !force_full || (level == Level::Full && focused.is_none())
}

/// Inert brackets for the policy predicate and the shell quoter.
///
/// These cannot launch a run or authorize a receipt — they only prove the
/// predicate refuses every non-qualifying case AND accepts the one qualifying
/// case, so it is not vacuously true. `validate.sh` ran the equivalent brackets
/// on every invocation (validate.sh:308); here they are a `--self-test` subcommand
/// so the cost is not paid on the hot path.
fn self_test() -> Result<(), String> {
    // CLI bracket: a real positive budget reaches the typed field, while zero,
    // negative, malformed, and missing values are all refused.
    let parsed = parse_argv(&["--run-timeout".into(), "600".into(), "--self-test".into()])
        .map_err(|code| format!("run-timeout parser refused 600s with exit {code}"))?;
    if parsed.run_timeout != Some(600) {
        return Err(format!(
            "run-timeout parser produced {:?}, expected 600s",
            parsed.run_timeout
        ));
    }
    for bad in ["0", "-1", "not-seconds"] {
        if parse_argv(&["--run-timeout".into(), bad.into(), "--self-test".into()]).is_ok() {
            return Err(format!("run-timeout parser accepted invalid value {bad:?}"));
        }
    }
    if parse_argv(&["--run-timeout".into()]).is_ok() {
        return Err("run-timeout parser accepted a missing value".into());
    }
    if scope_grace_s(600) != 60 || 600 + scope_grace_s(600) >= 720 {
        return Err("run-timeout scope backstop no longer satisfies 600 < 660 < 720".into());
    }
    // All three legitimate deadline sources share one pure precedence rule. The standalone boxed
    // re-exec must preserve D1 exactly; a scheduler epoch applies even when validate is top-level;
    // missing, future, and contradictory sources are refused.
    let now_ns = 10_000_000_000u64;
    let started_ns = 5_000_000_000u64;
    let allowance_ns = 600_000_000_000u64;
    let d1 = started_ns + allowance_ns;
    if deadline_from_sources(Some(600), true, false, None, None, now_ns).is_ok() {
        return Err("nested timeout accepted a missing scheduler-owned start epoch".into());
    }
    if deadline_from_sources(
        Some(600),
        true,
        false,
        Some(now_ns + 1),
        None,
        now_ns,
    )
    .is_ok()
    {
        return Err("nested timeout accepted a future scheduler-owned start epoch".into());
    }
    for nested in [false, true] {
        if deadline_from_sources(
            Some(600),
            nested,
            false,
            Some(started_ns),
            None,
            now_ns,
        )? != Some(d1)
        {
            return Err("scheduler epoch did not bind both top-level and nested deadlines".into());
        }
    }
    if deadline_from_sources(
        Some(600),
        true,
        true,
        Some(started_ns),
        Some(d1 - 1),
        now_ns,
    )? != Some(d1)
    {
        return Err("nested payload consumed its parent's scope deadline marker".into());
    }
    if deadline_from_sources(Some(600), false, true, None, Some(d1), now_ns)? != Some(d1) {
        return Err("boxed re-exec reset D1 instead of preserving it".into());
    }
    if deadline_from_sources(
        Some(600),
        false,
        true,
        Some(started_ns),
        Some(d1 + 1),
        now_ns,
    )
    .is_ok()
    {
        return Err("contradictory scheduler and scope deadline sources were accepted".into());
    }
    if deadline_from_sources(Some(600), false, false, None, Some(d1), now_ns)?
        != Some(now_ns + allowance_ns)
    {
        return Err("an out-of-scope marker forged deadline ownership".into());
    }
    let saved_scope_deadline = std::env::var_os(OWN_SCOPE_DEADLINE_ENV);
    for non_owner in [None, Some(""), Some("0"), Some("99"), Some("malformed")] {
        match non_owner {
            Some(v) => std::env::set_var(OWN_SCOPE_DEADLINE_ENV, v),
            None => std::env::remove_var(OWN_SCOPE_DEADLINE_ENV),
        }
        if owns_scope_request(Some(100)) {
            return Err(format!(
                "scope request ownership accepted non-owner marker {non_owner:?}"
            ));
        }
    }
    std::env::set_var(OWN_SCOPE_DEADLINE_ENV, "100");
    if !owns_scope_request(Some(100)) || owns_scope_request(None) {
        return Err("scope request ownership failed its exact positive bracket".into());
    }
    match saved_scope_deadline {
        Some(v) => std::env::set_var(OWN_SCOPE_DEADLINE_ENV, v),
        None => std::env::remove_var(OWN_SCOPE_DEADLINE_ENV),
    }

    // Positive: the one qualifying case must be ACCEPTED (guards against a
    // predicate that refuses everything and looks correct).
    if !force_full_policy_allows(true, Level::Full, None) {
        return Err("force-full: full/unfocused must be allowed".into());
    }
    if !force_full_policy_allows(false, Level::Quick, Some("rr-compat-only")) {
        return Err("force-full: inactive flag must allow anything".into());
    }
    // Negative: every non-full level and every focused mode must be REFUSED.
    for l in [Level::Quick, Level::PortableOnly, Level::Super] {
        if force_full_policy_allows(true, l, None) {
            return Err(format!("force-full: level {} must be refused", l.name()));
        }
    }
    for m in [
        "envelope-only",
        "strict-compat-only",
        "portable-strict-compat-only",
        "rr-compat-only",
        "sabre-compat-only",
        "e9patch-compat-only",
        "liteinst-compat-only",
        "qemu-l2-only",
        "privileged-only",
        "only",
        "selective",
        "shallow-select",
    ] {
        if force_full_policy_allows(true, Level::Full, Some(m)) {
            return Err(format!("force-full: focused mode {m} must be refused"));
        }
    }
    // Shell quoting: a corpus argv element must survive round-tripping through
    // `bash -c` byte-for-byte. A silent mangling here would change what the guest
    // runs while every count still looked right.
    for probe in [
        "plain",
        "with space",
        "single'quote",
        "$(command sub)",
        "back`tick`",
        "new\nline",
        r#"double"quote"#,
        "a;b|c&d",
        "",
    ] {
        let quoted = validate_plan::shell_quote(probe);
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!("printf '%s' {quoted}"))
            .output()
            .map_err(|e| format!("shell-quote bracket: cannot run bash: {e}"))?;
        let got = String::from_utf8_lossy(&out.stdout);
        if got != probe {
            return Err(format!("shell-quote bracket: {probe:?} round-tripped as {got:?}"));
        }
    }
    // Corpus tables must still match the counts the bash declared. This is the
    // drift guard for a MECHANICALLY EXTRACTED table: if someone edits a corpus
    // JSON without moving the corresponding ratchet, or vice versa, the extraction
    // has silently diverged from the numbers the gates are judged against.
    if validate_corpus::RR_PASSING_LABELS.len() != validate_corpus::RR_COMPAT_EXPECTED {
        return Err(format!(
            "R/R label set has {} rows, expected {}",
            validate_corpus::RR_PASSING_LABELS.len(),
            validate_corpus::RR_COMPAT_EXPECTED
        ));
    }
    let root = repo_root();
    let paths = validate_corpus::CorpusPaths {
        root_dir: "/nonexistent",
        real_compat_fixtures: "/nonexistent",
        validation_tmp_dir: "/nonexistent",
        shell_build_dir: "/nonexistent",
    };
    let count = |m: &str| -> Result<usize, String> {
        validate_corpus::load(&root, m, &paths).map(|r| r.len())
    };
    // Exact: these two matched their declared totals at extraction time, and that
    // exact agreement is the evidence the extraction was faithful.
    let strict = count("strict")?;
    if strict != validate_corpus::STRICT_COMPAT_TOTAL {
        return Err(format!(
            "strict corpus has {strict} rows, STRICT_COMPAT_TOTAL is {}",
            validate_corpus::STRICT_COMPAT_TOTAL
        ));
    }
    let sabre = count("sabre")?;
    if sabre != validate_corpus::SABRE_COMPAT_TOTAL {
        return Err(format!(
            "sabre corpus has {sabre} rows, SABRE_COMPAT_TOTAL is {}",
            validate_corpus::SABRE_COMPAT_TOTAL
        ));
    }
    // rr admits a superset and is filtered to the measured-passing labels; what
    // must hold is that every passing label is actually present to be measured.
    let rr_rows = validate_corpus::load(&root, "rr", &paths)?;
    let present: BTreeSet<&str> = rr_rows.iter().map(|r| r.label.as_str()).collect();
    let missing: Vec<&&str> = validate_corpus::RR_PASSING_LABELS
        .iter()
        .filter(|l| !present.contains(**l))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} R/R passing label(s) are absent from the rr corpus and could never be measured: {missing:?}",
            missing.len()
        ));
    }
    // e9patch admits a superset of its gated total (rows gate only when the
    // program is installed), so the invariant is >=, not ==.
    let e9 = count("e9patch")?;
    if e9 < validate_corpus::E9PATCH_COMPAT_TOTAL {
        return Err(format!(
            "e9patch corpus has {e9} rows, below E9PATCH_COMPAT_TOTAL {}",
            validate_corpus::E9PATCH_COMPAT_TOTAL
        ));
    }
    println!(
        "  corpora: strict={strict} sabre={sabre} rr={} (filtered to {}) e9patch={e9}",
        rr_rows.len(),
        validate_corpus::RR_COMPAT_EXPECTED
    );
    // Every ported subsystem brings its own two-sided brackets. They are inert:
    // none of them runs a gate, publishes a label, writes the real ledger, or
    // touches a PR — see each module's `self_test` doc for why that matters.
    for line in [
        validate_super::self_test(&root)?,
        validate_envelope::self_test()?,
        validate_history::self_test()?,
        validate_receipt::self_test()?,
        validate_runtime::self_test()?,
    ] {
        println!("  {line}");
    }
    // The `--envelope-*` CLI shape is a CONTRACT with scripts/progress-report.sh
    // and the progress-rubric skill, so it is asserted rather than assumed.
    envelope_cli_bracket()?;
    verbosity_cli_bracket(&root)?;
    super_plan_bracket()?;
    // Completeness is what a self-certifying driver is least able to check about
    // itself, so its refusal predicate is bracketed here rather than assumed.
    verdict_refusal_bracket()?;
    coverage_schema_bracket()?;
    selective_subset_bracket(&root)?;
    self_output_bracket()?;
    // ---- DAG-config carry + ungrantable-resource brackets -------------------
    // BOTH directions. A check that refuses everything would pass the negative
    // case alone, so the positive case (a real lane admits) is load-bearing.
    {
        let root = repo_root();
        for lane in ["portable", "privileged"] {
            let base = validate_plan::lane_config(&root, lane)?;
            // POSITIVE: a real lane's own config must carry, and must be grantable.
            let steps = validate_plan::lane_nodes(&root, lane, "", "gate.manifest")?;
            let carried = validate_plan::config_from_base(&base, steps, "bracket");
            validate_plan::assert_config_carried(&base, &carried)
                .map_err(|e| format!("carry bracket: lane {lane} did not carry its config: {e}"))?;
            if base.resource_caps.is_empty() {
                return Err(format!("carry bracket: lane {lane} declares no resource_caps; \
                                    the bracket would be vacuous"));
            }
            let bad = validate_plan::ungrantable_resources(&carried);
            if !bad.is_empty() {
                return Err(format!(
                    "grantable bracket: lane {lane} carried its caps yet still reports {} \
                     ungrantable demand(s): {:?}", bad.len(), &bad[..bad.len().min(3)]));
            }
            // NEGATIVE: drop the caps exactly as the bug did -> must be REFUSED,
            // and must NAME the resource rather than sleeping on it.
            let mut stripped = carried.clone();
            stripped.resource_caps.clear();
            let starved = validate_plan::ungrantable_resources(&stripped);
            if starved.is_empty() {
                return Err(format!(
                    "grantable bracket: lane {lane} with resource_caps CLEARED reported nothing \
                     ungrantable -- the check is inert and would not have caught the stall"));
            }
            let named = base.resource_caps.keys().any(|r| starved.iter().any(|b| b.contains(r)));
            if !named {
                return Err(format!("grantable bracket: refusal for {lane} names no resource: {:?}",
                                   &starved[..starved.len().min(2)]));
            }
            // NEGATIVE 2: a dropped config must be DETECTED, not tolerated.
            let defaulted = validate_plan::config_from(carried.steps.clone(), "bracket");
            if validate_plan::assert_config_carried(&base, &defaulted).is_ok() {
                return Err(format!(
                    "carry bracket: lane {lane} rebuilt from Default::default() compared EQUAL to \
                     its file config -- the assertion cannot detect the bug it exists for"));
            }
            println!("  dag-config: {lane} carries {} cap(s), default_step_timeout={}s; \
cleared-caps refusal names {} starved step(s)",
                     base.resource_caps.len(), base.default_step_timeout, starved.len());
        }
    }
    // The full hot path is one fused DAG and pays the exact-tree manifest audit
    // once. Bracket the positive shape and both diagnostic escape hatches: a
    // sequential plan still exists, while the nested audit reuse is accepted
    // only for the no-label portable-strict payload.
    {
        let root = repo_root();
        let tmp = std::env::temp_dir().join(format!("validate-plan-selftest-{}", std::process::id()));
        let full_args = parse_argv(&["full".into(), "--no-label-pr".into()])
            .map_err(|rc| format!("full-plan bracket: parser refused positive form rc={rc}"))?;
        let full = build_plan(&root, &full_args, &tmp)?;
        if full.second.is_some() {
            return Err("full-plan bracket: default full plan is still sequential".into());
        }
        let manifest_nodes: Vec<String> = full
            .cfg
            .steps
            .iter()
            .filter(|s| s.cmd == "./ci/test_harness.sh validate")
            .map(|s| s.tag())
            .collect();
        if manifest_nodes != vec!["gate.manifest"] {
            return Err(format!(
                "full-plan bracket: exact-tree manifest audit was not deduped to gate.manifest: {manifest_nodes:?}"
            ));
        }
        let pin_nodes: Vec<String> = full
            .cfg
            .steps
            .iter()
            .filter(|s| s.cmd.contains("ci/run-reverie-pin-check.sh"))
            .map(|s| s.tag())
            .collect();
        if pin_nodes != vec![PIN_GATE_TAG] {
            return Err(format!(
                "full-plan bracket: pin authority was not deduped to the observed preflight: {pin_nodes:?}"
            ));
        }
        for required in ["test.strict_compat", "privileged-cpuid.faulting"] {
            if !full.cfg.steps.iter().any(|s| s.tag() == required) {
                return Err(format!("full-plan bracket: fused plan lost {required}"));
            }
        }
        let canonical_test_tags = test_nodes_of(&validate_plan::lane_config(&root, "portable")?);
        let fused_test_tags = test_nodes_of(&full.cfg);
        if fused_test_tags != canonical_test_tags {
            return Err(format!(
                "full-plan bracket: fused tags changed the receipt coverage denominator: \
                 canonical={canonical_test_tags:?}, fused={fused_test_tags:?}"
            ));
        }
        let portable_build = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "build.workspace")
            .ok_or("full-plan bracket: portable fat build disappeared")?;
        if !portable_build.cmd.contains("cargo build --workspace --all-targets")
            || !portable_build.cmd.contains("cargo build -p hermit")
            || !portable_build.cmd.contains("--bin hermit")
        {
            return Err("full-plan bracket: fat build does not finish the debug Hermit producer".into());
        }
        let artifact = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "build.e2e_artifact")
            .ok_or("full-plan bracket: verified E2E artifact publisher disappeared")?;
        if !artifact.cmd.contains("ci/publish-hermit-e2e-artifact.sh")
            || !artifact.cmd.ends_with(" target/install_pkg")
            || !["build.workspace", "build.runtime_release"]
                .iter()
                .all(|dep| artifact.deps.iter().any(|actual| actual == dep))
        {
            return Err(
                "full-plan bracket: E2E publisher is not a complete binary+resource barrier"
                    .into(),
            );
        }
        let manifest_consumers: Vec<_> = full
            .cfg
            .steps
            .iter()
            .filter(|s| s.cmd.contains("./ci/test_harness.sh run --lane "))
            .collect();
        if manifest_consumers.is_empty() {
            return Err("full-plan bracket: no manifest consumers were inspected".into());
        }
        for consumer in manifest_consumers {
            if !consumer.cmd.starts_with("./ci/run-with-hermit-e2e-artifact.sh ") {
                return Err(format!(
                    "full-plan bracket: {} still consumes a mutable Hermit path: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            let producer = if consumer.cmd.contains("--lane portable ") {
                if !consumer.cmd.contains("--require-install") {
                    return Err(format!(
                        "full-plan bracket: portable consumer {} did not require the backend-resource bundle",
                        consumer.tag()
                    ));
                }
                "build.e2e_artifact"
            } else {
                "privileged-build.privileged_tests"
            };
            if !consumer.deps.iter().any(|d| d == producer) {
                return Err(format!(
                    "full-plan bracket: {} does not declare immutable artifact producer {producer}",
                    consumer.tag()
                ));
            }
        }
        let privileged_build = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "privileged-build.privileged_tests")
            .ok_or("full-plan bracket: privileged focused build disappeared")?;
        if !privileged_build
            .deps
            .iter()
            .any(|d| d == "build.e2e_artifact")
        {
            return Err(
                "full-plan bracket: privileged build can race the portable artifact producer"
                    .into(),
            );
        }
        if privileged_build.cmd.contains("cargo ")
            || !privileged_build
                .cmd
                .contains("verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path")
            || !privileged_build.cmd.contains("tests_misc-*")
        {
            return Err(
                "full-plan bracket: privileged build did not become an exact artifact assertion"
                    .into(),
            );
        }
        let cpuid = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "privileged-cpuid.faulting")
            .ok_or("full-plan bracket: privileged CPUID node disappeared")?;
        if cpuid.cmd.contains("cargo ") || !cpuid.cmd.contains("rdrand_rdseed_is_masked") {
            return Err(
                "full-plan bracket: CPUID test does not directly execute the prebuilt binary"
                    .into(),
            );
        }
        let sequential_args = parse_argv(&[
            "full".into(),
            "--sequential-lanes".into(),
            "--no-label-pr".into(),
        ])
        .map_err(|rc| format!("full-plan bracket: sequential diagnostic refused rc={rc}"))?;
        if build_plan(&root, &sequential_args, &tmp)?.second.is_none() {
            return Err("full-plan bracket: --sequential-lanes did not preserve the fallback".into());
        }
        let nested_args = parse_argv(&[
            "--portable-strict-compat-only".into(),
            "--reuse-parent-manifest-gate".into(),
            "--no-label-pr".into(),
        ])
        .map_err(|rc| format!("full-plan bracket: nested positive form refused rc={rc}"))?;
        let nested = build_plan(&root, &nested_args, &tmp)?;
        if nested.cfg.steps.iter().any(|s| s.tag() == "gate.manifest")
            || !nested.cfg.steps.iter().any(|s| s.tag() == PIN_GATE_TAG)
        {
            return Err(
                "full-plan bracket: nested reuse did not remove only manifest while retaining the pin gate"
                    .into(),
            );
        }
        if parse_argv(&[
            "--portable-strict-compat-only".into(),
            "--reuse-parent-manifest-gate".into(),
        ])
        .is_ok()
        {
            return Err("full-plan bracket: nested reuse accepted a label-capable invocation".into());
        }
        println!(
            "  full plan: {} fused node(s), 1 exact-tree manifest audit + 1 pin authority; sequential fallback + nested no-label reuse bracketed",
            full.cfg.steps.len()
        );
    }

    Ok(())
}

/// Bind the current schema to the evidence the row actually carries.
///
/// A missing or malformed coverage judgement stays explicit `null`. It must not
/// cause a new row to masquerade as a grandfathered schema-4 receipt.
fn ledger_schema_and_coverage(
    coverage: serde_json::Value,
) -> (i64, serde_json::Value) {
    let has_real_judgement = coverage
        .get("planned_test_nodes")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|planned| planned > 0);
    if has_real_judgement {
        (COVERAGE_LEDGER_SCHEMA_VERSION, coverage)
    } else {
        (COVERAGE_LEDGER_SCHEMA_VERSION, serde_json::Value::Null)
    }
}

/// Two-sided producer bracket for [`ledger_schema_and_coverage`]. Inert: it
/// serializes no row and writes no ledger.
fn coverage_schema_bracket() -> Result<(), String> {
    let real = serde_json::json!({
        "planned_test_nodes": 4,
        "executed_test_nodes": 4,
        "zero_executed_nodes": [],
        "absent_nodes": [],
    });
    let (schema, carried) = ledger_schema_and_coverage(real.clone());
    if schema != COVERAGE_LEDGER_SCHEMA_VERSION || carried != real {
        return Err("coverage schema: a real judgement must be carried as schema 5".into());
    }

    for unresolved in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!({"planned_test_nodes": 0}),
        serde_json::json!({"planned_test_nodes": "4"}),
    ] {
        let (schema, carried) = ledger_schema_and_coverage(unresolved);
        if schema != COVERAGE_LEDGER_SCHEMA_VERSION || !carried.is_null() {
            return Err(
                "coverage schema: unresolved evidence must remain schema 5 with null coverage".into(),
            );
        }
    }
    println!(
        "  coverage schema: 1/1 real judgement -> schema 5; 4/4 unresolved shapes -> schema 5/null"
    );
    Ok(())
}

/// Bracket the self-output classifier that decides whether the tree is dirty.
///
/// This predicate is load-bearing in a way that is easy to miss: `tree_dirty()`
/// feeds `commit_anchored`, which gates BOTH the tree-keyed cache and receipt
/// publication. When it was wrong, both features were inert and nothing said so
/// — every run simply recorded `commit_anchored: false` and re-ran. So each
/// listing SHAPE gets an explicit case, including the exact one that regressed:
/// a porcelain line whose leading status column has been eaten by a trim.
fn self_output_bracket() -> Result<(), String> {
    // MUST be excused (validate's own output, in every shape a caller emits).
    let excused = [
        (" M ci/validate-ledger/local.example-host.jsonl", "porcelain, modified, leading space intact"),
        ("M ci/validate-ledger/local.example-host.jsonl", "porcelain whose leading space a trim ate"),
        ("?? ci/validate-ledger/local.other.jsonl", "porcelain, untracked shard"),
        ("ci/validate-ledger/local.example-host.jsonl", "bare path (git diff --name-only)"),
        ("ignored/validate/validate-full-abc-1.log", "bare path, durable log"),
        (" M \"ci/validate-ledger/has space.jsonl\"", "porcelain, quoted path"),
        ("R  ci/validate-ledger/a.jsonl -> ci/validate-ledger/b.jsonl", "rename within the ledger dir"),
    ];
    for (line, why) in excused {
        if !line_is_self_output(line) {
            return Err(format!("self-output: {line:?} ({why}) must be excused as validate's own"));
        }
    }
    // MUST NOT be excused. A predicate that excused everything would satisfy the
    // list above and silently disable the dirty gate entirely.
    let foreign = [
        (" M scripts/validate.rs", "a real source change"),
        ("?? detcore/src/new_thing.rs", "a new untracked source file"),
        ("M  Cargo.lock", "a staged lockfile change"),
        ("scripts/lib/validate_plan.rs", "bare path, real source"),
        ("R  detcore/src/a.rs -> ci/validate-ledger/a.rs", "a source file MOVED into the ledger dir"),
        ("R  ci/validate-ledger/a.jsonl -> detcore/src/a.rs", "a ledger file moved OUT into source"),
        (" M ci/dag/portable.json", "a lane change under ci/, but not the ledger"),
        (" M ci/validate-ledger-notes.md", "a sibling whose name merely starts the same way"),
    ];
    for (line, why) in foreign {
        if line_is_self_output(line) {
            return Err(format!("self-output: {line:?} ({why}) must count as a DIRTY tree"));
        }
    }
    // LIVE invariant, independent of the synthetic shapes above: whatever this
    // checkout's real state is, no surviving entry may be validate's own output.
    // This is what actually catches a reintroduced trim, because it exercises
    // the real `git` invocation rather than a hand-written line.
    let mut live = 0usize;
    for args in [
        vec!["status", "--porcelain"],
        vec!["diff", "--name-only"],
        vec!["ls-files", "--others", "--exclude-standard"],
    ] {
        for line in foreign_porcelain(&args) {
            live += 1;
            if path_readings(&line).iter().any(|p| is_self_output(p)) {
                return Err(format!(
                    "self-output: `git {}` leaked validate's own output into the dirty set: {line:?}",
                    args.join(" ")
                ));
            }
        }
    }
    println!(
        "  self-output: {} own-output shape(s) excused, {} foreign change(s) still dirty, \
         {live} live entr(y/ies) from the real checkout all correctly classified",
        excused.len(),
        foreign.len()
    );
    Ok(())
}

/// Bracket the `--selective` subset builder against the REAL portable lane.
///
/// The dangerous failure here is silent under-running: a subset that drops a
/// node the selector asked for, or keeps a dangling dependency that makes the
/// runner skip a selected node. Both are checked against `ci/dag/portable.json`
/// itself rather than a fixture, because a fixture would not notice the lane
/// file changing shape underneath the selector.
fn selective_subset_bracket(root: &Path) -> Result<(), String> {
    let all = validate_plan::lane_nodes(root, "portable", "", "gate.manifest")?;
    let all_tags: BTreeSet<String> = all.iter().map(|s| s.tag()).collect();
    // Pick a node that has at least one intra-lane dependency, plus that
    // dependency, so the "keep both" and "prune the rest" behaviours are both
    // exercised on real data.
    let (child, parent) = all
        .iter()
        .find_map(|s| {
            s.deps.iter().find(|d| all_tags.contains(*d)).map(|d| (s.tag(), d.clone()))
        })
        .ok_or("selective bracket: ci/dag/portable.json has no intra-lane dependency to test")?;
    let keep: BTreeSet<String> = [child.clone(), parent.clone()].into_iter().collect();
    let sel = validate_plan::select_lane_nodes(all.clone(), &keep);
    // Positive: exactly the two named nodes survive, the kept edge survives, and
    // the manifest-gate edge (outside the lane) is NOT pruned.
    if sel.steps.len() != 2 {
        return Err(format!("selective bracket: kept {} node(s), expected 2", sel.steps.len()));
    }
    let kept_child = sel
        .steps
        .iter()
        .find(|s| s.tag() == child)
        .ok_or("selective bracket: the selected child node was dropped")?;
    if !kept_child.deps.contains(&parent) {
        return Err("selective bracket: a dependency inside the selected set must survive".into());
    }
    if sel.unknown_tags != Vec::<String>::new() {
        return Err(format!("selective bracket: unexpected unknown tags {:?}", sel.unknown_tags));
    }
    let root_node = sel.steps.iter().find(|s| s.tag() == parent).unwrap();
    if !root_node.deps.iter().all(|d| !all_tags.contains(d)) {
        return Err("selective bracket: an unselected lane dependency was left dangling".into());
    }
    // Negative: a tag the lane does not contain must be REPORTED, because that
    // means the selector and the DAG disagree and the subset is untrustworthy.
    let bogus: BTreeSet<String> =
        [parent.clone(), "no.such_node".to_string()].into_iter().collect();
    let sel2 = validate_plan::select_lane_nodes(all, &bogus);
    if sel2.unknown_tags != vec!["no.such_node".to_string()] {
        return Err(format!(
            "selective bracket: an unknown tag MUST be reported; got {:?}",
            sel2.unknown_tags
        ));
    }
    println!(
        "  selective subset: kept {child} + its dep {parent} from the real portable lane \
         ({} edge(s) pruned); 1 unknown-tag refusal",
        sel.pruned_edges
    );
    Ok(())
}

/// Assert that `super` plans a complete, fully-boxed suite — and that the audit
/// which guarantees that would actually REFUSE an unboxed node.
///
/// The caps audit is the driver's own load-bearing guard: it is what makes
/// "boxing ACTIVE" true for every node rather than for the ones someone
/// remembered. A guard that never fires is indistinguishable from no guard, so
/// this brackets it on both sides with an inert synthetic node.
fn super_plan_bracket() -> Result<(), String> {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("validate-super-plan-{}", std::process::id()));
    let args = parse_argv(&["super".to_string()])
        .map_err(|c| format!("super plan: the `super` level was REFUSED with exit {c}"))?;
    let plan = build_plan(&root, &args, &tmp)
        .map_err(|e| format!("super plan: could not build a plan: {e}"))?;
    // Positive: the audit must ACCEPT a real, fully-declared super plan.
    let undeclared = validate_plan::undeclared_nodes(&plan.cfg);
    if !undeclared.is_empty() {
        return Err(format!(
            "super plan: {} node(s) lack declared caps: {}",
            undeclared.len(),
            undeclared.join(", ")
        ));
    }
    let tags: BTreeSet<String> = plan.cfg.steps.iter().map(|s| s.tag()).collect();
    // One representative of each expansion the table names, so a lost synthetic
    // is caught here and not at 2am in the weekly run.
    for want in [
        "super.build_workspace",
        "super.build_release_hermit",
        "super.sqlite_veryquick_strict_determinism",
        "super.pmu_analyze_hello_race_stress_calibrated_skid",
        "superstress.ptrace_strict_verify_01",
        "superstress.kvm_available",
        "compatprep.fixtures",
        "compat.rustc",
    ] {
        if !tags.contains(want) {
            return Err(format!("super plan: node {want} is missing"));
        }
    }
    if !plan.super_mode {
        return Err("super plan: super_mode must be set so the stress table is printed".into());
    }
    // Negative: one node with no caps must be REFUSED by the same audit.
    let mut broken = validate_plan::config_from(
        vec![safe_ci_dag_runner::model::Step {
            group: "bracket".into(),
            job: "uncapped".into(),
            desc: "inert fixture: declares no caps".into(),
            description: String::new(),
            cmd: "true".into(),
            deps: vec![],
            env: BTreeMap::new(),
            hint: Default::default(),
            networkonly: false,
            engine_only: false,
            timeout: 0,
            cpu_timeout: 0,
            jobs_flag: None,
        }],
        "caps-audit negative bracket",
    );
    broken.default_step_cpu_timeout = 0;
    let refused = validate_plan::undeclared_nodes(&broken);
    if refused != vec!["bracket.uncapped".to_string()] {
        return Err(format!(
            "caps audit: an uncapped node MUST be refused; the audit returned {refused:?}"
        ));
    }
    println!(
        "  super plan: {} boxed node(s), all capped; caps audit bracketed 1 accept / 1 refusal",
        plan.cfg.steps.len()
    );
    Ok(())
}

fn verbosity_cli_bracket(root: &Path) -> Result<(), String> {
    let level = |args: &[&str]| -> Result<i64, String> {
        parse_argv(&args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
            .map(|a| a.verbosity)
            .map_err(|code| format!("verbosity argv {args:?} refused with exit {code}"))
    };
    if level(&["--verbose"])? != 2 {
        return Err("verbosity: --verbose must select level 2".into());
    }
    for expected in 1..=5 {
        if level(&["--verbosity", &expected.to_string()])? != expected {
            return Err(format!("verbosity: --verbosity {expected} did not round-trip"));
        }
    }
    for bad in ["0", "6", "loud"] {
        if parse_verbosity(bad).is_ok() {
            return Err(format!("verbosity: invalid level {bad:?} was accepted"));
        }
    }
    let args = parse_argv(&["full".into(), "--no-label-pr".into()])
        .map_err(|code| format!("verbosity: full-plan argv refused with exit {code}"))?;
    let mut plan = build_plan(root, &args, &std::env::temp_dir().join("validate-verbosity-bracket"))?;
    propagate_verbosity(&mut plan, 5);
    let missing = plan
        .cfg
        .steps
        .iter()
        .chain(plan.second.iter().flat_map(|cfg| cfg.steps.iter()))
        .filter(|step| step.env.get("VALIDATE_VERBOSITY").map(String::as_str) != Some("5"))
        .count();
    if missing != 0 {
        return Err(format!("verbosity: {missing} DAG child(ren) lost level 5"));
    }
    Ok(())
}

/// Assert the `--envelope-only` / `--envelope-compare FILE` surface, and that it
/// actually plans the envelope measurement.
///
/// `scripts/progress-report.sh:102` runs `./scripts/validate.rs --envelope-only` and the
/// progress-rubric skill runs it with `ENVELOPE_JSON=...`. Those callers break
/// silently if the flag stops being accepted or starts meaning something else.
/// The parser and planner are exercised in-process, so the bracket measures the
/// FLAG SURFACE and not the checkout's cleanliness.
fn envelope_cli_bracket() -> Result<(), String> {
    let argv = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("validate-envelope-cli-{}", std::process::id()));
    // Positive: both spellings must be ACCEPTED, select the envelope profile,
    // and produce a plan containing the L4 stress node — a parser that accepted
    // the flag and planned nothing would satisfy a weaker check.
    let mut accepted = 0usize;
    for v in [vec!["--envelope-only"], vec!["--envelope-compare", "/nonexistent-baseline.json"]] {
        let args = parse_argv(&argv(&v))
            .map_err(|c| format!("envelope CLI: `{v:?}` was REFUSED with exit {c}"))?;
        if !matches!(args.focused, Some(Focused::Envelope { .. })) {
            return Err(format!("envelope CLI: `{v:?}` did not select the envelope mode"));
        }
        let plan = build_plan(&root, &args, &tmp)
            .map_err(|e| format!("envelope CLI: `{v:?}` could not build a plan: {e}"))?;
        if plan.profile != "envelope-only" {
            return Err(format!("envelope CLI: `{v:?}` recorded profile {}", plan.profile));
        }
        let tags: BTreeSet<String> = plan.cfg.steps.iter().map(|s| s.tag()).collect();
        for want in ["envelope.build", "envelope.true_l4", "envelope.date_rr"] {
            if !tags.contains(want) {
                return Err(format!("envelope CLI: `{v:?}` planned no {want} node"));
            }
        }
        if !plan.force_keep_going {
            return Err("envelope CLI: the measurement must force keep-going".into());
        }
        if plan.nonblocking.len() != validate_envelope::PROBES.len() * validate_envelope::LEVELS.len()
        {
            return Err(format!(
                "envelope CLI: {} probe node(s) must be nonblocking, found {}",
                validate_envelope::PROBES.len() * validate_envelope::LEVELS.len(),
                plan.nonblocking.len()
            ));
        }
        // The build node must NOT be excused: it is the one gate in this profile.
        if plan.nonblocking.contains("envelope.build") {
            return Err("envelope CLI: the workspace build must stay BLOCKING".into());
        }
        // The measurement must never be answered from the tree-keyed cache: the
        // vector is an artifact consumers re-read, and with a baseline the
        // verdict depends on a file that is not part of the key.
        if plan.cacheable {
            return Err("envelope CLI: the envelope profile must NOT be cacheable".into());
        }
        accepted += 1;
    }
    // Negative: a missing FILE must be refused, not silently defaulted, and the
    // mode must not combine with a level, --all, or another focused mode.
    let mut refused = 0usize;
    for (why, v) in [
        ("--envelope-compare with no FILE", vec!["--envelope-compare"]),
        ("--envelope-only combined with a level", vec!["quick", "--envelope-only"]),
        ("--envelope-only combined with --all", vec!["--all", "--envelope-only"]),
        ("--envelope-only combined with another focused mode", vec!["--envelope-only", "--rr-compat-only"]),
    ] {
        if parse_argv(&argv(&v)).is_ok() {
            return Err(format!("envelope CLI: {why} must be REFUSED"));
        }
        refused += 1;
    }
    // Both spellings are ONE mode, so combining them is legal and the baseline
    // wins — this is the case validate.sh accepted (ENVELOPE_MODE=only twice).
    match parse_argv(&argv(&["--envelope-only", "--envelope-compare", "b.json"]))
        .map_err(|c| format!("envelope CLI: the two spellings must combine, got exit {c}"))?
        .focused
    {
        Some(Focused::Envelope { baseline: Some(_) }) => accepted += 1,
        other => return Err(format!("envelope CLI: combined spellings gave {other:?}")),
    }
    println!("  envelope CLI: {accepted} accepted form(s), {refused} refused misuse(s) (the \
              refusal messages above are expected)");
    Ok(())
}

// --------------------------------------------------------------------------- jobs

/// Default scheduler width, honoring the same runtime authority `validate.sh`
/// used (validate.sh:692-716) so both pick identical widths on the same host:
/// an explicit `CI_DAG_JOBS` is used EXACTLY (no clamp); otherwise the
/// host-adaptive `host_cpus/8`, floored at 2 and capped at 16.
///
/// The cap is measurement-backed, not a guess: on this 316-CPU box the portable
/// DAG measured CPU/wall ~2.6x at -j2 versus ~21.8x at -j16, and becomes
/// critical-path-bound near width 16. The same file also runs on GitHub's ~4-CPU
/// portable runner, where a flat 16 would schedule many multi-GiB nodes at once
/// and OOM a job that -j2 kept green.
fn default_jobs() -> i64 {
    if let Ok(v) = std::env::var("CI_DAG_JOBS") {
        if !v.is_empty() {
            if let Ok(n) = v.parse::<i64>() {
                if n > 0 {
                    return n;
                }
            }
            eprintln!("validate: CI_DAG_JOBS={v:?} is not a positive integer; using the host-adaptive default");
        }
    }
    let host = std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(1);
    (host / 8).clamp(2, 16)
}

// --------------------------------------------------------------------------- boxing

/// How much longer than validate's own budget the scope may live.
///
/// The scope is only a backstop for the driver itself wedging. Validate needs
/// this later window to reap nodes and flush its rows, so it must not be the
/// level that normally fires. At the strict-compat 600s run budget this is 60s,
/// establishing the configured 600 < 660 portion of the nesting ladder.
fn scope_grace_s(run_timeout_s: i64) -> i64 {
    60.max(run_timeout_s / 10)
}

fn owns_scope_request(deadline_ns: Option<u64>) -> bool {
    deadline_ns.is_some_and(|deadline| {
        std::env::var(OWN_SCOPE_DEADLINE_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            == Some(deadline)
    })
}

/// Establish two-level cgroup-v2 boxing, mirroring the runner's own
/// `resolve_cgroups` policy. Returns the manager (`None` = intentional unboxed
/// run) or `Err(exit_code)`. On the default path this re-execs into a transient
/// `systemd --user` scope and does not return on success.
fn resolve_cgroups(
    allow_failure: bool,
    run_timeout_s: Option<i64>,
    deadline_ns: Option<u64>,
) -> Result<BoxedCgroups, u8> {
    if is_in_scope() {
        // A RuntimeMaxSec request is only this invocation's backstop when the marker carries this
        // invocation's absolute deadline. Nested validates inherit an outer scope and its env;
        // treating that inherited request as the nested 660s rung would compare unrelated clocks.
        if owns_scope_request(deadline_ns) {
            let verified = expected_scope_runtime_max_s()
                .is_some_and(verify_scope_runtime_max);
            if !verified {
                let msg = "this invocation's outer RuntimeMaxSec readback failed; the requested \
                           scope backstop is not proven on the live unit";
                if allow_failure {
                    eprintln!(
                        "validate: WARNING: {msg}; running UNBOXED (--allow-cgroup-failure)."
                    );
                    return Ok(None);
                }
                eprintln!("validate: ERROR: {msg}.");
                return Err(3);
            }
        } else if run_timeout_s.is_some() {
            eprintln!(
                "validate: inherited cgroup scope has no invocation-owned RuntimeMaxSec rung; \
                 the anchored in-process deadline remains inside the enclosing DAG node limit"
            );
        }
        let mgr = Cgroups::new();
        if mgr.enabled() {
            install_scope_teardown();
            eprintln!(
                "validate: cgroup boxing ACTIVE (two-level cgroup-v2 scope; per-step memory/CPU \
                 caps + setsid-proof teardown)."
            );
            return Ok(Some(Arc::new(mgr) as Arc<dyn CgroupManager>));
        }
        if allow_failure {
            eprintln!("validate: WARNING: per-step cgroup setup failed; running UNBOXED (--allow-cgroup-failure).");
            return Ok(None);
        }
        eprintln!(
            "validate: ERROR: inside a managed scope but per-step cgroups could not be set up; \
             re-run with --allow-cgroup-failure to run UNBOXED."
        );
        return Err(3);
    }
    if allow_failure {
        eprintln!(
            "validate: WARNING: cgroup boxing not established (--allow-cgroup-failure); running \
             UNBOXED (process-group teardown only, no per-step memory/CPU caps)."
        );
        return Ok(None);
    }
    let scope_runtime_s = run_timeout_s.and_then(|run| {
        remaining_budget_s(deadline_ns).map(|remaining| remaining + scope_grace_s(run))
    });
    if let Some(deadline) = deadline_ns {
        std::env::set_var(OWN_SCOPE_DEADLINE_ENV, deadline.to_string());
    } else {
        std::env::remove_var(OWN_SCOPE_DEADLINE_ENV);
    }
    let attempt = attempt_scope_reexec(None, None, scope_runtime_s);
    let detail = attempt.describe();
    eprintln!(
        "validate: ERROR: cgroup boxing could not be established: {detail}. Resource boxing is \
         this tool's primary purpose; re-run with --allow-cgroup-failure to run UNBOXED."
    );
    Err(3)
}

// --------------------------------------------------------------------------- durable log

/// A live self-tee: everything written to fd 1/2 is duplicated into a durable
/// absolute log AND still shown on the terminal.
///
/// The receipt path must not depend on the launch path. A bare
/// `./scripts/validate.rs` with no `ci-hub validate-run` unit around it would
/// otherwise run, pass, and leave nothing on disk — indistinguishable from never
/// having run. Teeing here means the log exists whether the run came from
/// `validate-run`, `make validate`, or a bare invocation.
struct DurableLog {
    path: PathBuf,
    tee: std::process::Child,
    orig_stdout: i32,
    orig_stderr: i32,
}

impl DurableLog {
    fn finish(mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // Restoring fds 1/2 drops the last pipe write-ends, so tee sees EOF.
        unsafe {
            libc::dup2(self.orig_stdout, 1);
            libc::dup2(self.orig_stderr, 2);
            libc::close(self.orig_stdout);
            libc::close(self.orig_stderr);
        }
        let _ = self.tee.wait();
    }
}

/// Durable log path. Always ABSOLUTE — `verify_receipt.sh` (the merge gate)
/// requires the recorded path to start with `/`. Never under `HERMIT_DIR`: that
/// is a user-facing setting and validation must not write there.
fn durable_log_path(root: &Path, profile: &str, sha: &str) -> PathBuf {
    let dir = match std::env::var(PARENT_ENV) {
        Ok(p) if !p.is_empty() => PathBuf::from(p).join("ignored").join("validate"),
        _ => root.join("ignored").join("validate"),
    };
    let sha12: String = sha.chars().take(12).collect();
    let ts = utc_now().replace([':', '-'], "");
    dir.join(format!("validate-{profile}-{sha12}-{ts}.log"))
}

/// Establish the self-tee. FAIL-CLOSED: any failure exits loudly rather than
/// running without a durable receipt. Must be called AFTER `resolve_cgroups`
/// (which re-execs), so the tee is set up once, in the final boxed process.
fn setup_durable_log(root: &Path, profile: &str, sha: &str) -> Result<DurableLog, u8> {
    use std::os::unix::io::AsRawFd;
    let path = durable_log_path(root, profile, sha);
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "validate: ERROR: cannot create durable-log dir {}: {e}. A run with no durable \
                 receipt is a silent no-result; refusing to proceed.",
                dir.display()
            );
            return Err(4);
        }
    }
    let mut tee = match Command::new("tee")
        .arg("-a")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "validate: ERROR: cannot spawn `tee` for {}: {e}. Refusing to run without a \
                 durable receipt.",
                path.display()
            );
            return Err(4);
        }
    };
    let (orig_stdout, orig_stderr, ok) = unsafe {
        let so = libc::dup(1);
        let se = libc::dup(2);
        let pipe_fd = tee.stdin.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
        let ok = so >= 0
            && se >= 0
            && pipe_fd >= 0
            && libc::dup2(pipe_fd, 1) >= 0
            && libc::dup2(pipe_fd, 2) >= 0;
        (so, se, ok)
    };
    if !ok {
        eprintln!("validate: ERROR: could not redirect stdout/stderr into the durable log.");
        let _ = tee.kill();
        return Err(4);
    }
    drop(tee.stdin.take());
    eprintln!("validate: durable log: {}", path.display());
    Ok(DurableLog { path, tee, orig_stdout, orig_stderr })
}

// --------------------------------------------------------------------------- git / host

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn git_sha() -> String {
    sh("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into())
}

/// Content-addressed identity of exactly what validate builds and tests: the root
/// tree object. It hashes tracked file content AND submodule gitlink SHAs, but not
/// commit metadata — so a rebase or amend that leaves content byte-identical
/// yields the SAME tree. This, not the commit SHA, is the result-cache key.
fn git_tree() -> String {
    sh("git", &["rev-parse", "HEAD^{tree}"]).unwrap_or_else(|| "unknown".into())
}

fn repo_root() -> PathBuf {
    sh("git", &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Paths excluded from every dirtiness and anchoring judgement.
///
/// The ledger shard lives IN the repository, and validate is what writes it. If
/// it counted as dirt, validate would poison the very tree it just judged: the
/// next run would refuse on a dirty tree, and the tree hash — the result-cache
/// key — would change after every run, so a cache could never hit. Validate's own
/// output is not a source change, so it is excluded here rather than being
/// gitignored (the shards are meant to be committed and unioned across machines).
const SELF_OUTPUT_PREFIXES: &[&str] = &[LEDGER_DIR, "ignored/"];

/// True when `path` is inside (or equal to) one of validate's own output roots.
///
/// The match is on a PATH BOUNDARY, not a raw string prefix. A bare
/// `starts_with("ci/validate-ledger")` also swallowed siblings such as
/// `ci/validate-ledger-notes.md`, which would have been silently excused from the
/// dirty gate — the opposite of the failure it is meant to prevent, and exactly
/// the kind of "correlated proxy" match this driver is supposed to avoid.
fn is_self_output(path: &str) -> bool {
    SELF_OUTPUT_PREFIXES.iter().any(|p| {
        let root = p.trim_end_matches('/');
        path == root || path.starts_with(&format!("{root}/"))
    })
}

/// Every path a git listing line could be referring to.
///
/// The callers emit two different shapes — `git status --porcelain` prefixes each
/// path with a two-character status plus a space, while `git diff --name-only`
/// and `git ls-files` emit a bare path — and a rename line carries two paths.
/// Rather than guess which caller produced a line, every plausible reading is
/// derived and the classification asks whether ALL of them are validate's own
/// output.
///
/// **Do not reintroduce a fixed-offset strip.** Two bugs have now come from one:
/// stripping three characters unconditionally broke the bare-path callers
/// (turning `ci/validate-ledger/…` into `validate-ledger/…`), and the fix for
/// that still relied on the porcelain line keeping its leading status column —
/// which `sh()` trimmed off the FIRST line of the output. The measured effect of
/// the second bug: after any run, `git status --porcelain` returned exactly one
/// line, ` M ci/validate-ledger/<shard>.jsonl`, whose leading space `sh()` ate;
/// the 3-char strip then produced `i/validate-ledger/…`, no reading matched, and
/// `tree_dirty()` reported TRUE. Every subsequent ledger row was written with
/// `commit_anchored: false`, so the tree-keyed cache could never hit and a
/// receipt-backed label could never be published — both features inert, silently.
fn path_readings(line: &str) -> Vec<String> {
    let unquote = |s: &str| s.trim().trim_matches('"').to_string();
    let mut out = vec![unquote(line)];
    if let Some(rest) = porcelain_payload(line) {
        out.push(unquote(rest));
    }
    // Belt and braces for the exact bug this replaced: a porcelain line whose
    // leading status column was eaten by a trim reads as `M <path>`. Reading it
    // costs nothing (an extra reading can only WIDEN "self output", and the two
    // prefixes are specific paths) and it means a future accidental trim
    // degrades to "still classified correctly" instead of "cache silently off".
    const CODES: &[u8] = b"MADRCUT?!";
    let b = line.as_bytes();
    if b.len() > 2 && b[1] == b' ' && CODES.contains(&b[0]) {
        out.push(unquote(&line[2..]));
    }
    out
}

/// If `line` has a `git status --porcelain` `XY ` prefix, the text after it.
///
/// Both status characters are checked against git's actual code set rather than
/// just testing for a space at index 2, so an ordinary path that happens to
/// contain a space in its third position is not mistaken for a status prefix.
fn porcelain_payload(line: &str) -> Option<&str> {
    const CODES: &[u8] = b" MADRCUT?!";
    let b = line.as_bytes();
    if b.len() > 3 && b[2] == b' ' && CODES.contains(&b[0]) && CODES.contains(&b[1]) {
        Some(&line[3..])
    } else {
        None
    }
}

/// True when this listing line describes only validate's own output.
///
/// A rename (`R  old -> new`) counts as self-output only when BOTH sides are:
/// moving a source file INTO the ledger directory is a real change and must not
/// be excused.
fn line_is_self_output(line: &str) -> bool {
    let payload: &str = porcelain_payload(line).unwrap_or(line);
    if let Some((from, to)) = payload.split_once(" -> ") {
        let clean = |s: &str| s.trim().trim_matches('"').to_string();
        return is_self_output(&clean(from)) && is_self_output(&clean(to));
    }
    path_readings(line).iter().any(|p| is_self_output(p))
}

/// Entries from a git listing that are not validate's own output.
///
/// Reads git's stdout UNTRIMMED, because `git status --porcelain`'s leading
/// status column is significant and a global trim silently shifts the first
/// line's columns (see [`path_readings`]).
fn foreign_porcelain(args: &[&str]) -> Vec<String> {
    let Ok(out) = Command::new("git").args(args).output() else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !line_is_self_output(l))
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// True when the tree differs from HEAD in any way validate did not itself cause.
fn tree_dirty() -> bool {
    !foreign_porcelain(&["status", "--porcelain"]).is_empty()
}

/// True when the WORKING TREE proper carries changes `git add` would capture.
/// This drives the hard gate, because staging or committing is the caller's
/// escape from it.
fn worktree_dirty() -> bool {
    let unstaged = !foreign_porcelain(&["diff", "--name-only"]).is_empty();
    unstaged || !foreign_porcelain(&["ls-files", "--others", "--exclude-standard"]).is_empty()
}

fn utc_now() -> String {
    sh("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".into())
}

fn epoch_now() -> i64 {
    sh("date", &["+%s"]).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn has_cmd(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate the dev-hermit parent by walking up for a `.gitmodules` whose `hermit`
/// submodule path is `hermit` (validate.sh:19).
fn find_parent(root: &Path) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    loop {
        if cur.join(".gitmodules").is_file() {
            if let Some(p) = sh(
                "git",
                &[
                    "-C",
                    cur.to_str()?,
                    "config",
                    "-f",
                    ".gitmodules",
                    "--get",
                    "submodule.hermit.path",
                ],
            ) {
                if p == "hermit" {
                    return Some(cur);
                }
            }
        }
        if !cur.pop() || cur.as_os_str().is_empty() {
            return None;
        }
    }
}

/// `validation_slot_name` (validate.sh:37): which worktree slot this checkout is.
fn slot_name(root: &Path, parent: Option<&Path>) -> String {
    let Some(parent) = parent else { return "standalone".into() };
    let Ok(rel) = root.strip_prefix(parent) else { return "standalone".into() };
    let rel = rel.to_string_lossy();
    if rel == "hermit" {
        return "primary".into();
    }
    if let Some(rest) = rel.strip_prefix("worktrees/") {
        if let Some((slot, _)) = rest.split_once('/') {
            return slot.to_string();
        }
    }
    "standalone".into()
}

/// Classify the build-cache state BEFORE anything is built. Warm vs cold target/
/// dominates wall time, so the estimate and the ledger both record it.
fn cache_state(root: &Path) -> &'static str {
    let debug = root.join("target/debug/hermit").exists();
    let release = root.join("target/release/hermit").exists();
    match (debug, release) {
        (true, true) => "warm",
        (true, false) | (false, true) => "partial",
        (false, false) => "cold",
    }
}

// --------------------------------------------------------------------------- rebase freshness

/// Refuse to validate a head that is behind its upstream.
///
/// Owner directive: "ALWAYS rebase before validate; admission control should
/// ERROR if the base is out of date." The reason is not tidiness — a receipt is
/// keyed to a SHA, and while a stale head waits, `main` advances and the receipt
/// stops describing anything landable. Validating a stale base spends the
/// box-exclusive validate slot producing evidence that is already invalid.
///
/// Only ERRORS when the local `origin/main` ref genuinely contains commits this
/// head lacks. It does NOT fetch (that would make an offline run fail for a
/// network reason) and it does not fire when the ref is absent — an unknown base
/// is reported as unknown, never silently treated as fresh.
fn rebase_freshness(force: bool) -> Result<String, String> {
    if sh("git", &["rev-parse", "--verify", "--quiet", "refs/remotes/origin/main"]).is_none() {
        return Ok("base: origin/main not present locally; freshness UNKNOWN (not asserted)".into());
    }
    let counts = sh("git", &["rev-list", "--left-right", "--count", "origin/main...HEAD"])
        .unwrap_or_else(|| "0\t0".into());
    let mut it = counts.split_whitespace();
    let behind: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let ahead: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    if behind == 0 {
        return Ok(format!("base: up to date with origin/main (ahead {ahead}, behind 0)"));
    }
    let msg = format!(
        "HEAD is {behind} commit(s) BEHIND origin/main (ahead {ahead}).\n  \
         A receipt minted here is keyed to a SHA that main has already moved past, so it cannot \
         authorize a landing and will have to be rebuilt after the rebase it is missing.\n  \
         Rebase first:  git rebase origin/main\n  \
         To validate a deliberately stale base anyway, pass --run-on-dirty-tree."
    );
    if force {
        Ok(format!("base: STALE, {behind} behind origin/main — forced past the freshness gate"))
    } else {
        Err(msg)
    }
}

// --------------------------------------------------------------------------- plan

/// What the driver will execute, plus the accounting the ledger needs.
struct Plan {
    cfg: DagConfig,
    /// Second DAG run for a two-lane profile when lanes are NOT fused. Keeping
    /// them sequential is the faithful reproduction of `run_full_suite`, which
    /// runs `run_ci_manifest_lane portable` then `... privileged`.
    second: Option<DagConfig>,
    profile: String,
    selection_mode: &'static str,
    /// `test.*` nodes the profile PLANNED to run, for the coverage record.
    #[allow(dead_code)]
    planned_test_nodes: BTreeSet<String>,
    /// Set when this profile is a compatibility matrix, so the ratchet and the
    /// per-program summary are evaluated afterwards.
    compat: Option<CompatMode>,
    /// True only for a complete `full` plan, authorizing `gates_expected` to be
    /// derived from what ran (validate.sh:718).
    suite_complete: bool,
    /// True for the `super` stress suite, so its pass-rate table is printed and
    /// its verdict comes from the ratchet rather than the raw node count.
    super_mode: bool,
    /// Set for `--envelope-only`/`--envelope-compare`: the measurement is scored
    /// and emitted afterwards, and an optional baseline is enforced.
    envelope: Option<EnvelopePlan>,
    /// Tags whose failure must NOT turn the run red. This is how a MEASUREMENT
    /// (envelope probes) and a NEVER-BEFORE-MEASURED row (KVM/DBI stress) are
    /// kept out of the blocking verdict without hiding them from the report.
    /// Every member is named in the summary with the reason it is nonblocking.
    nonblocking: BTreeSet<String>,
    /// Forced on for the envelope profile, whose whole point is to measure every
    /// probe: an eager exit on the first probe failure would truncate the vector.
    force_keep_going: bool,
    /// May a prior passing record for this tree be reused instead of running?
    ///
    /// The tree-keyed cache is only sound when the run is a pure function of the
    /// tree. The envelope profile is neither: its verdict under
    /// `--envelope-compare FILE` depends on a BASELINE FILE that is not part of
    /// the key, and its purpose under `--envelope-only` is to (re)produce the
    /// `envelope.json` ARTIFACT that `scripts/progress-report.sh` then reads — a
    /// cache hit would answer a monotonicity question it never asked and leave
    /// the artifact unwritten. `validate.sh` cached it anyway (its cache gate at
    /// :655 runs before the `ENVELOPE_MODE` dispatch at :4877, with
    /// `VALIDATION_PROFILE=envelope-only`); that is a bug, not a contract.
    cacheable: bool,
}

struct EnvelopePlan {
    reps: i64,
    baseline: Option<PathBuf>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            cfg: DagConfig::default(),
            second: None,
            profile: String::new(),
            selection_mode: "full",
            planned_test_nodes: BTreeSet::new(),
            compat: None,
            suite_complete: false,
            super_mode: false,
            envelope: None,
            nonblocking: BTreeSet::new(),
            force_keep_going: false,
            cacheable: true,
        }
    }
}

fn test_nodes_of(cfg: &DagConfig) -> BTreeSet<String> {
    cfg.steps
        .iter()
        .map(|s| s.tag())
        .filter(|t| t.starts_with("test.") || t.contains(":test."))
        .collect()
}

/// Build the execution plan for the selected level/mode.
fn build_plan(root: &Path, args: &Args, tmp: &Path) -> Result<Plan, String> {
    let with_proxy = has_cmd("with-proxy");
    let pre = validate_plan::preflight_nodes(root, with_proxy);
    let gate = "gate.manifest";

    // Focused compatibility matrices.
    let compat_mode = match &args.focused {
        Some(Focused::StrictCompat) => Some(CompatMode::Strict),
        Some(Focused::PortableStrictCompat) => Some(CompatMode::PortableStrict),
        Some(Focused::SabreCompat) => Some(CompatMode::Sabre),
        Some(Focused::E9patchCompat) => Some(CompatMode::E9patch),
        Some(Focused::RrCompat) => Some(CompatMode::Rr),
        _ => None,
    };
    if let Some(mode) = compat_mode {
        let hermit_bin = std::env::var("STRICT_COMPAT_HERMIT_BIN")
            .unwrap_or_else(|_| root.join("target/release/hermit").to_string_lossy().into());
        let fixtures = root.join(format!("target/real-compat-fixtures-{}", std::process::id()));
        let nsswitch = tmp.join("e9patch-nsswitch.conf");
        let shell_build = tmp.join("shell-build");
        let paths = validate_corpus::CorpusPaths {
            root_dir: &root.to_string_lossy(),
            real_compat_fixtures: &fixtures.to_string_lossy(),
            validation_tmp_dir: &tmp.to_string_lossy(),
            shell_build_dir: &shell_build.to_string_lossy(),
        };
        let mut steps = pre;
        let compat_gate = if args.reuse_parent_manifest_gate {
            // The outer node is reachable only after its real gate.manifest
            // passed. Avoid rerunning that ~75 s exact-tree audit inside the
            // nested payload, but retain the cheap, independently observed
            // submodule and pin gates.
            steps.retain(|s| s.tag() != gate);
            PIN_GATE_TAG
        } else {
            gate
        };
        // The corpus needs a release Hermit and the functional fixtures; both are
        // DAG nodes so they are boxed and timed like everything else.
        steps.push(build_release_hermit_node(compat_gate, &hermit_bin));
        steps.push(prepare_fixtures_node("compatprep.fixtures", &fixtures));
        if mode == CompatMode::E9patch {
            steps.push(nsswitch_fixture_node(&nsswitch));
        }
        steps.extend(validate_plan::compat_nodes(
            root,
            mode,
            &hermit_bin,
            &nsswitch.to_string_lossy(),
            &paths,
            Some("compatprep.fixtures"),
        )?);
        let profile = args.focused.as_ref().unwrap().profile();
        let cfg = validate_plan::config_from(steps, &format!("compatibility matrix: {mode:?}"));
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            second: None,
            profile,
            selection_mode: "full",
            compat: Some(mode),
            ..Default::default()
        });
    }

    // Focused single-shard mode: run one already-built DAG shard, no deps.
    if let Some(Focused::Only { lane, nodes }) = &args.focused {
        let mut steps = pre;
        steps.push(shard_node(gate, lane, nodes));
        let cfg = validate_plan::config_from(steps, "single DAG shard");
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            second: None,
            profile: args.focused.as_ref().unwrap().profile(),
            selection_mode: "only",
            ..Default::default()
        });
    }

    // Focused liteinst matrix (validate.sh:4815): three ordered gates.
    if matches!(args.focused, Some(Focused::LiteinstCompat)) {
        let mut steps = pre;
        steps.push(step_with_caps("liteinst", "hermit_release", "Release Hermit for LiteInst compatibility",
            "cargo build --release --locked -p hermit --features third-party-backends".into(),
            vec![gate.to_string()], 1200, 3600, 16 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("liteinst", "runtime", "Release LiteInst runtime",
            "./scripts/stage-liteinst-runtime.sh release $PWD/target/release/libreverie_liteinst.so $PWD/target/liteinst-runtime-build".into(),
            vec!["liteinst.hermit_release".into()], 900, 1800, 8 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("liteinst", "strict", "Portable CI liteinst_strict",
            "HERMIT_LITEINST_TEST_BINARY=$PWD/target/release/hermit cargo test -p hermit --features third-party-backends --test liteinst_advanced -- --test-threads=1".into(),
            vec!["liteinst.runtime".into()], 900, 1800, 8 * 1024 * 1024 * 1024));
        let cfg = validate_plan::config_from(steps, "liteinst compatibility");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: args.focused.as_ref().unwrap().profile(), selection_mode: "full",
            ..Default::default() });
    }

    // Focused QEMU L2 boot (validate.sh:4860). Heavyweight; two ordered gates.
    if matches!(args.focused, Some(Focused::QemuL2)) {
        let mut steps = pre;
        steps.push(step_with_caps("qemu", "hermit_release", "Release Hermit for QEMU L2",
            "cargo build --release -p hermit --features third-party-backends".into(),
            vec![gate.to_string()], 3600, 7200, 16 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("qemu", "strict_l2_boot", "QEMU strict L2 boot (heavyweight)",
            "./tests/qemu-boot/strict_l2_test.sh".into(),
            vec!["qemu.hermit_release".into()], 1500, 3000, 16 * 1024 * 1024 * 1024));
        let cfg = validate_plan::config_from(steps, "QEMU L2 boot");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: args.focused.as_ref().unwrap().profile(), selection_mode: "full",
            ..Default::default() });
    }

    // `quick` is NOT "the portable lane" — it is seven specific smoke gates
    // (validate.sh:4583). Mapping it onto a lane would run a different, much
    // larger thing under the same name.
    if args.level == Level::Quick && args.focused.is_none() {
        let hermit = "target/debug/hermit";
        let marker = "hermit-validation-smoke";
        let run_args = "run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled";
        let mut steps = pre;
        let mut add = |job: &str, desc: &str, cmd: String, dep: &str, t: i64, mem: i64| {
            steps.push(step_with_caps("quick", job, desc, cmd, vec![dep.to_string()], t, t * 2, mem));
        };
        add("build", "Build workspace", "cargo build --workspace --features third-party-backends".into(), gate, 3600, 16 * 1024 * 1024 * 1024);
        add("e2e_metadata", "Portable E2E metadata", "./ci/test_harness.sh validate".into(), "quick.build", 600, 4 * 1024 * 1024 * 1024);
        add("e2e_verify", "Portable ptrace E2E verification", "./ci/test_harness.sh run --lane portable --mode verify --backend ptrace --ci-only".into(), "quick.build", 1800, 8 * 1024 * 1024 * 1024);
        add("detcore_unit", "Detcore core unit tests", "cargo test -p hermit-detcore --lib".into(), "quick.build", 1800, 8 * 1024 * 1024 * 1024);
        add("run_smoke", "Hermit run smoke test",
            format!("out=$(timeout 30s {hermit} {run_args} -- /bin/echo {marker}) && test \"$out\" = {marker}"),
            "quick.build", 120, 4 * 1024 * 1024 * 1024);
        add("verify_smoke", "Hermit verify-mode smoke test",
            format!("timeout 30s {hermit} {run_args} --verify -- /bin/echo {marker}"),
            "quick.build", 120, 4 * 1024 * 1024 * 1024);
        add("record_replay_smoke", "Hermit record/replay smoke test",
            format!("timeout 30s {hermit} record start --verify -- /bin/echo {marker}"),
            "quick.build", 180, 4 * 1024 * 1024 * 1024);
        let cfg = validate_plan::config_from(steps, "quick smoke suite");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: "quick".into(), selection_mode: "full", ..Default::default() });
    }

    // The `super` stress/diagnostic suite (validate.sh:4702).
    if args.level == Level::Super && args.focused.is_none() {
        return super_plan(root, tmp, pre, gate);
    }

    // Working-envelope measurement (validate.sh:4173). A MEASUREMENT, not a
    // gate: probe failures lower a count and never abort, so keep-going is
    // forced and every probe node is nonblocking.
    if let Some(Focused::Envelope { baseline }) = &args.focused {
        let reps = validate_envelope::l4_reps();
        let hermit_bin = root.join("target/debug/hermit").to_string_lossy().into_owned();
        let mut steps = pre;
        steps.push(validate_envelope::build_node(gate));
        let probes = validate_envelope::nodes(&hermit_bin, reps, "envelope.build");
        let nonblocking: BTreeSet<String> = probes.iter().map(|s| s.tag()).collect();
        steps.extend(probes);
        let cfg = validate_plan::config_from(steps, "working-envelope measurement");
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            profile: "envelope-only".into(),
            envelope: Some(EnvelopePlan { reps, baseline: baseline.clone() }),
            nonblocking,
            force_keep_going: true,
            cacheable: false,
            ..Default::default()
        });
    }

    // Node-level `--selective` / `--since-green` (validate.sh:4421).
    if let Some(Focused::Selective { shallow }) = &args.focused {
        return selective_plan(root, args, pre, gate, *shallow);
    }

    // Lane-based profiles.
    let lanes: Vec<&str> = match (&args.focused, args.level) {
        (Some(Focused::PrivilegedOnly), _) => vec!["privileged"],
        (None, Level::PortableOnly) => vec!["portable"],
        (None, Level::Full) => vec!["portable", "privileged"],
        (_, _) => {
            return Err(format!(
                "no plan is defined for level={:?} focused={:?}; refusing to substitute another profile",
                args.level, args.focused
            ))
        }
    };
    let profile = match &args.focused {
        Some(f) => f.profile(),
        None => args.level.name().to_string(),
    };
    let selection_mode = match &args.focused {
        Some(Focused::Selective { .. }) => "selective",
        Some(Focused::Only { .. }) => "only",
        _ => "full",
    };

    if lanes.len() == 2 && !args.merge_lanes {
        // Faithful reproduction of run_full_suite: portable lane, then privileged.
        let mut a = pre.clone();
        a.extend(validate_plan::lane_nodes(root, lanes[0], "", gate)?);
        let mut b = validate_plan::lane_nodes(root, lanes[1], "", gate)?;
        // The second run repeats preflight-free; its nodes hang off nothing.
        for s in b.iter_mut() {
            s.deps.retain(|d| d != gate);
        }
        // Each lane carries ITS OWN loaded config. They genuinely differ --
        // portable default_step_timeout=600 vs privileged=120, and disjoint
        // resource_caps -- so there is no correct single merged value; running
        // them as two sequential DAGs lets each keep its own exactly.
        let base_a = validate_plan::lane_config(root, lanes[0])?;
        let base_b = validate_plan::lane_config(root, lanes[1])?;
        let cfg_a = validate_plan::config_from_base(&base_a, a, "portable lane");
        let cfg_b = validate_plan::config_from_base(&base_b, b, "privileged lane");
        for (base, derived, lane) in [(&base_a, &cfg_a, lanes[0]), (&base_b, &cfg_b, lanes[1])] {
            validate_plan::assert_config_carried(base, derived)
                .map_err(|e| format!("lane {lane}: DAG config was not carried: {e}"))?;
        }
        let mut planned = test_nodes_of(&cfg_a);
        planned.extend(test_nodes_of(&cfg_b));
        return Ok(Plan {
            cfg: cfg_a,
            second: Some(cfg_b),
            profile,
            selection_mode,
            planned_test_nodes: planned,
            suite_complete: args.level == Level::Full && args.focused.is_none(),
            ..Default::default()
        });
    }

    let mut steps = pre;
    for lane in &lanes {
        // Keep the portable lane's shipped tags byte-identical: the
        // main-reachable receipt finalizer derives its coverage denominator
        // from those manifest tags. Prefix only the additional lane, which is
        // sufficient to disambiguate every collision in the fused graph.
        let prefix = if lanes.len() > 1 && *lane != "portable" {
            format!("{lane}-")
        } else {
            String::new()
        };
        steps.extend(validate_plan::lane_nodes(root, lane, &prefix, gate)?);
    }
    // Fusing lanes can duplicate identical work. In particular, the always-on
    // gate.manifest and both lane e2e.metadata nodes run the exact same
    // `test_harness.sh validate` tree audit. Drop later duplicates and repoint
    // their dependents, so one full run pays that ~75 s audit exactly once. The
    // dedup is exact-command based for this one audited command and is reported.
    let removed = dedupe_identical(&mut steps);
    if !removed.is_empty() {
        eprintln!("validate: fused lanes; deduped {} identical node(s): {}", removed.len(), removed.join(", "));
    }
    if lanes.len() == 2 {
        // The artifact barrier waits for both initial Cargo producers, verifies
        // binary and resource identities, then publishes a content-addressed
        // bundle. Every later Cargo writer and manifest consumer runs only after
        // that barrier, so no writer can mutate either source during publication
        // and no consumer reads a mutable Cargo path afterward.
        let producer = "build.e2e_artifact";
        let debug_producer = "build.workspace";
        let consumer = "privileged-build.privileged_tests";
        let portable_build = steps
            .iter()
            .find(|s| s.tag() == debug_producer)
            .ok_or_else(|| format!("fused debug producer disappeared: {debug_producer}"))?;
        let expected_fat_build = "./ci/run-with-reverie-dbt-budget.sh cargo build --workspace --all-targets --features third-party-backends && CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit";
        if portable_build.cmd != expected_fat_build {
            return Err(format!(
                "fused debug producer command drifted; re-prove the artifact barrier: {}",
                portable_build.cmd
            ));
        }
        let artifact = steps
            .iter()
            .find(|s| s.tag() == producer)
            .ok_or_else(|| format!("fused artifact producer disappeared: {producer}"))?;
        let expected_artifact = "./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path target/install_pkg";
        if artifact.cmd != expected_artifact
            || ![debug_producer, "build.runtime_release"]
                .iter()
                .all(|dep| artifact.deps.iter().any(|actual| actual == dep))
        {
            return Err(format!(
                "fused artifact barrier drifted; re-prove binary+resource publication: {} deps={:?}",
                artifact.cmd, artifact.deps
            ));
        }
        let privileged_build = steps
            .iter_mut()
            .find(|s| s.tag() == consumer)
            .ok_or_else(|| format!("fused artifact consumer disappeared: {consumer}"))?;
        let expected_build = "CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit && ./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path && CARGO_BUILD_JOBS=8 cargo test -p hermit-detcore --test tests_misc --no-run";
        if privileged_build.cmd != expected_build {
            return Err(format!(
                "fused privileged build command drifted; re-prove that build.workspace is a superset: {}",
                privileged_build.cmd
            ));
        }
        if !privileged_build.deps.iter().any(|d| d == producer) {
            privileged_build.deps.push(producer.to_string());
            privileged_build.deps.sort();
        }
        // SELECT THE NEWEST, DO NOT REQUIRE EXACTLY ONE.
        //
        // This assertion used to end `test "$count" -eq 1`, and that made the
        // owner's `make validate` fail EVERY TIME while passing in every agent
        // worktree. Cargo writes one hash-suffixed `tests_misc-<hash>` per build
        // and never prunes the old ones, so the count is 1 only in a FRESH or
        // just-`cargo clean`ed tree. Measured 2026-08-10: 9 executables in
        // ~/work/dev-hermit/hermit versus 1 in a cleaned slot. `test 9 -eq 1`
        // exits 1 instantly and the shell builtin prints nothing -- which is
        // exactly the "0s, exit 1" with an empty detail block seen in both
        // failing runs at 2b38d8e6. It is not flaky and it is not a timeout:
        // once a working tree accumulates a second binary it can never pass
        // again. We validated only in clean clones, i.e. the one condition
        // where the defect cannot appear.
        //
        // Fixing the CHECK rather than the user's working directory is
        // deliberate: this must work in any checkout, including a dirty one,
        // and validate must not delete a developer's build artifacts.
        //
        // Newest-by-mtime is what cargo itself would run. Deliberately NOT
        // relaxed to `-ge 1`: the CPUID consumer below executes the binary it
        // selects, so "any one of nine" would let it silently test a STALE
        // artifact -- a check that passes while measuring the wrong thing,
        // which is worse than failing loudly. Zero binaries still fails.
        privileged_build.cmd = "./ci/verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path >/dev/null || exit 1; newest=\"\"; for f in target/debug/deps/tests_misc-*; do if [ -f \"$f\" ] && [ -x \"$f\" ] && { [ -z \"$newest\" ] || [ \"$f\" -nt \"$newest\" ]; }; then newest=\"$f\"; fi; done; test -n \"$newest\"".to_string();

        let cpuid = steps
            .iter_mut()
            .find(|s| s.tag() == "privileged-cpuid.faulting")
            .ok_or("fused prebuilt CPUID consumer disappeared")?;
        let expected_cpuid = "timeout 30 cargo test -p hermit-detcore --test tests_misc rdrand_rdseed_is_masked -- --exact";
        if cpuid.cmd != expected_cpuid {
            return Err(format!(
                "fused CPUID command drifted; re-prove direct prebuilt invocation: {}",
                cpuid.cmd
            ));
        }
        // Same defect, same fix: `((${#bins[@]} == 1))` failed for exactly the
        // reason above, so this node could never run in a long-lived checkout
        // either. It EXECUTES the binary it picks, which is precisely why the
        // selection must be the NEWEST rather than an arbitrary survivor of a
        // `-ge 1` relaxation -- running a stale `tests_misc` would report a
        // CPUID verdict about an artifact that is not the one under test.
        cpuid.cmd = "newest=\"\"; for f in target/debug/deps/tests_misc-*; do if [ -f \"$f\" ] && [ -x \"$f\" ] && { [ -z \"$newest\" ] || [ \"$f\" -nt \"$newest\" ]; }; then newest=\"$f\"; fi; done; test -n \"$newest\"; timeout 30 \"$newest\" rdrand_rdseed_is_masked --exact".to_string();
    }
    // Fusing lanes means one config for both. Their default wall timeouts differ,
    // but every shipped/synthesized node has an explicit wall timeout and the
    // fail-closed undeclared-node audit below enforces that invariant. Therefore
    // the default is unreachable; retain the stricter value as defense in depth.
    // Resource caps are disjoint and merge cleanly.
    let bases: Vec<DagConfig> = lanes
        .iter()
        .map(|l| validate_plan::lane_config(root, l))
        .collect::<Result<_, _>>()?;
    let mut fused = bases[0].clone();
    for b in bases.iter().skip(1) {
        fused.default_step_timeout = fused.default_step_timeout.min(b.default_step_timeout);
        for (r, n) in &b.resource_caps {
            if let Some(prev) = fused.resource_caps.get(r) {
                if prev != n {
                    return Err(format!(
                        "--merge-lanes refused: resource {r} capped at {prev} and {n} by different lanes"
                    ));
                }
            }
            fused.resource_caps.insert(r.clone(), *n);
        }
    }
    let cfg = validate_plan::config_from_base(&fused, steps, "fused lanes");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        second: None,
        profile,
        selection_mode,
        suite_complete: args.level == Level::Full && args.focused.is_none(),
        ..Default::default()
    })
}

/// Build the `super` plan from the mechanically extracted gate table.
///
/// Dependency policy — the bash ran all 32 rows strictly sequentially through
/// `run_check`, so ANY edge set that preserves the real prerequisites is a
/// faithful port and a strictly better schedule. The prerequisites are:
///   * the two build rows gate everything that needs a binary;
///   * `run_exact_detcore_cases` is FAIL-FAST within its group
///     (validate.sh:4514), reproduced by chaining those rows so a failure SKIPS
///     the rest instead of running them;
///   * the LevelDB test needs its fixture built first.
/// Everything else is independent and is allowed to overlap.
fn super_plan(
    root: &Path,
    tmp: &Path,
    pre: Vec<safe_ci_dag_runner::model::Step>,
    gate: &str,
) -> Result<Plan, String> {
    let gates = validate_super::load_gates(root)?;
    let reps = validate_super::repetitions();
    let build_ws = "super.build_workspace".to_string();
    let build_rel = "super.build_release_hermit".to_string();
    let debug_bin = root.join("target/debug/hermit").to_string_lossy().into_owned();
    let release_bin = std::env::var("STRICT_COMPAT_HERMIT_BIN")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| root.join("target/release/hermit").to_string_lossy().into_owned());

    let mut steps = pre;
    let mut nonblocking: BTreeSet<String> = BTreeSet::new();
    // `run_exact_detcore_cases` labels its rows "<group>: <case>"; consecutive
    // rows sharing a group prefix are one fail-fast family. Deriving the chain
    // from the label SHAPE keeps it correct if a case is added or removed.
    let family = |label: &str| label.split_once(": ").map(|(g, _)| g.to_string());
    let mut prev_family: Option<(String, String)> = None; // (family, previous tag)

    for g in &gates {
        let deps = match g.job.as_str() {
            "build_workspace" | "build_release_hermit" => vec![gate.to_string()],
            "full_leveldb_strict_determinism" => {
                vec!["super.build_pinned_leveldb_super_fixture".to_string()]
            }
            _ => vec![build_ws.clone()],
        };
        match g.synthetic.as_deref() {
            Some("portable_slow_strict_diagnostics") => {
                // The four PORTABLE_STRICT_SUPER_ONLY workloads, run with the
                // portable-strict flags after the shared functional fixtures are
                // prepared (validate.sh:4603).
                let fixtures = root.join(format!("target/real-compat-fixtures-{}", std::process::id()));
                steps.push(prepare_fixtures_node_dep("compatprep.fixtures", &fixtures, &build_rel));
                let only: BTreeSet<String> =
                    validate_corpus::portable_super_only().keys().map(|k| k.to_string()).collect();
                let shell_build = tmp.join("shell-build");
                let paths = validate_corpus::CorpusPaths {
                    root_dir: &root.to_string_lossy(),
                    real_compat_fixtures: &fixtures.to_string_lossy(),
                    validation_tmp_dir: &tmp.to_string_lossy(),
                    shell_build_dir: &shell_build.to_string_lossy(),
                };
                steps.extend(validate_plan::compat_nodes_for(
                    root,
                    CompatMode::PortableStrict,
                    &release_bin,
                    "",
                    &paths,
                    Some("compatprep.fixtures"),
                    Some(&only),
                    Some(g.wall()),
                )?);
            }
            Some("super_stress_suite") => {
                let stress =
                    validate_super::stress_nodes(&release_bin, &debug_bin, tmp, reps, &build_rel, &build_ws);
                steps.extend(stress);
                nonblocking.extend(validate_super::nonblocking_tags(reps));
            }
            Some("calibrated_analyze_tests") => {
                steps.push(validate_super::calibrated_analyze_node(g, deps));
            }
            Some(other) => {
                return Err(format!(
                    "ci/super/gates.json row {} names an unknown synthetic expansion `{other}`; \
                     refusing to skip it silently",
                    g.job
                ))
            }
            None => {
                // Fail-fast chaining inside a `run_exact_detcore_cases` family.
                let mut deps = deps;
                if let Some(f) = family(&g.label) {
                    if let Some((pf, ptag)) = &prev_family {
                        if *pf == f {
                            deps = vec![ptag.clone()];
                        }
                    }
                    prev_family = Some((f, format!("super.{}", g.job)));
                } else {
                    prev_family = None;
                }
                steps.push(validate_super::gate_node(g, deps));
            }
        }
    }
    let cfg = validate_plan::config_from(steps, "super stress + diagnostic suite");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        profile: "super".into(),
        super_mode: true,
        nonblocking,
        ..Default::default()
    })
}

/// What `ci/select-tests.rs` decided, and what that means for the plan.
enum SelectDecision {
    /// No CI-relevant change: run nothing beyond preflight.
    Skip,
    /// Run exactly this dependency-closed node set.
    Nodes(BTreeSet<String>),
    /// Fail-safe: run the complete portable lane, for the stated reason.
    Full(String),
}

/// Ask `ci/select-tests.rs` what to run.
///
/// This is PLAN CONSTRUCTION, not a gate: the selector produces no verdict about
/// the tree, and its output is only used to choose which already-declared nodes
/// to schedule. Every failure mode — a nonzero exit, unparseable JSON, an empty
/// node set, or an unproducible coverage report — resolves to
/// [`SelectDecision::Full`], so the driver can only ever err toward running MORE
/// than the selector proved safe to omit (validate.sh:4416-4420).
fn ask_selector(root: &Path, baseline: Option<&str>) -> SelectDecision {
    let run = |format: &str| -> Option<String> {
        let mut c = Command::new(root.join("ci").join("select-tests.rs"));
        c.arg("--since-green");
        if let Some(b) = baseline {
            c.args(["--baseline", b]);
        }
        c.args(["--format", format]);
        let out = c.output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let Some(json_text) = run("json") else {
        return SelectDecision::Full("select-tests.rs failed".into());
    };
    let Ok(sel) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return SelectDecision::Full("select-tests.rs emitted unparseable JSON".into());
    };
    // A subset must never run without a human-auditable account of what it
    // dropped and why, so an unproducible report is treated as doubt.
    let report = run("human").unwrap_or_default();
    if report.trim().is_empty() {
        return SelectDecision::Full("could not produce the coverage report".into());
    }
    println!("----- selective coverage report (skipped nodes/shards/e2e cells + reasons) -----");
    println!("{}", report.trim_end());
    println!("-------------------------------------------------------------------------------");
    match sel.get("decision").and_then(|d| d.as_str()).unwrap_or("full") {
        "skip" => SelectDecision::Skip,
        "selective" => {
            let nodes: BTreeSet<String> = sel
                .get("nodes")
                .and_then(|n| n.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
                .unwrap_or_default();
            if nodes.is_empty() {
                SelectDecision::Full("empty selected node set".into())
            } else {
                SelectDecision::Nodes(nodes)
            }
        }
        other => SelectDecision::Full(format!("decision={other}")),
    }
}

/// Build the `--selective` plan (validate.sh:4421).
fn selective_plan(
    root: &Path,
    args: &Args,
    pre: Vec<safe_ci_dag_runner::model::Step>,
    gate: &str,
    shallow: bool,
) -> Result<Plan, String> {
    let commit_exists =
        |sha: &str| sh("git", &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_some()
            || Command::new("git")
                .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
    let baseline: Option<String> = if shallow {
        // --shallow-select pins the baseline to HEAD~1. A root commit has no
        // parent, so selection fails safe to the full lane (validate.sh:4369).
        sh("git", &["rev-parse", "--verify", "HEAD~1"])
    } else {
        let ledger = ledger_path(root);
        let rows = validate_history::read_rows(&ledger);
        let parent = find_parent(root);
        let slot = slot_name(root, parent.as_deref());
        validate_history::selective_baseline(&rows, args.baseline.as_deref(), &slot, &commit_exists)
    };
    match &baseline {
        Some(b) => println!("Selective validation: last-known-green baseline = {b}"),
        None => println!(
            "Selective validation: no trustworthy green baseline; running the FULL portable lane."
        ),
    }

    let all = validate_plan::lane_nodes(root, "portable", "", gate)?;
    let total = all.len();
    let decision = match &baseline {
        Some(b) => ask_selector(root, Some(b)),
        None => SelectDecision::Full("no trustworthy green baseline".into()),
    };
    let steps: Vec<safe_ci_dag_runner::model::Step> = match decision {
        SelectDecision::Skip => {
            println!(
                "Selective validation: no CI-relevant changes since baseline — nothing to run \
                 (0/{total} nodes). Preflight still ran; the ledger's coverage record will show \
                 zero planned test nodes, so this cannot be misread as a full pass."
            );
            Vec::new()
        }
        SelectDecision::Nodes(keep) => {
            let sel = validate_plan::select_lane_nodes(all, &keep);
            if !sel.unknown_tags.is_empty() {
                return Err(format!(
                    "select-tests.rs named {} node(s) absent from ci/dag/portable.json ({}); the \
                     selector and the DAG disagree, so refusing to run a subset derived from a \
                     stale mapping",
                    sel.unknown_tags.len(),
                    sel.unknown_tags.join(", ")
                ));
            }
            println!(
                "Selective validation: running {}/{total} portable DAG nodes ({} intra-lane \
                 dependency edge(s) pruned to the selected set):\n  {}",
                sel.steps.len(),
                sel.pruned_edges,
                keep.iter().cloned().collect::<Vec<_>>().join(" ")
            );
            sel.steps
        }
        SelectDecision::Full(why) => {
            println!("Selective validation: {why} — running the FULL portable lane.");
            all
        }
    };
    let mut nodes = pre;
    nodes.extend(steps);
    let cfg = validate_plan::config_from(nodes, "selective portable subset");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        profile: "selective".into(),
        selection_mode: "selective",
        ..Default::default()
    })
}

/// Remove later steps whose semantic work exactly matches an earlier step's,
/// and repoint every dependency onto the survivor. Returns the removed tags.
///
/// Most nodes require both job and command to match. Deliberate exceptions are
/// the manifest audit (different tags, byte-identical command/tree) and the
/// Reverie-pin authority (preflight passes `--repo`, lane nodes rely on the same
/// root cwd). The observed preflight node survives in both cases.
fn dedupe_identical(steps: &mut Vec<safe_ci_dag_runner::model::Step>) -> Vec<String> {
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut keep = Vec::with_capacity(steps.len());
    let mut removed = Vec::new();
    for s in steps.drain(..) {
        let tag = s.tag();
        let key = if s.cmd == "./ci/test_harness.sh validate" {
            (
                "exact-tree-manifest-audit".to_string(),
                "./ci/test_harness.sh validate".to_string(),
            )
        } else if [
            "pre.reverie_pin",
            "check.reverie_pin",
            "privileged-check.reverie_pin",
        ]
        .contains(&tag.as_str())
            && s.cmd.contains("ci/run-reverie-pin-check.sh")
        {
            // The preflight spells the repository explicitly while lane nodes
            // rely on the same root cwd. They invoke the same single pin
            // authority; retaining the preflight observation also keeps
            // `reverie_pin_current` evidence-derived.
            (
                "reverie-pin-authority".to_string(),
                "current-repository-pin".to_string(),
            )
        } else {
            (s.job.clone(), s.cmd.clone())
        };
        match seen.get(&key) {
            Some(surv) => {
                remap.insert(s.tag(), surv.clone());
                removed.push(s.tag());
            }
            None => {
                seen.insert(key, s.tag());
                keep.push(s);
            }
        }
    }
    for s in keep.iter_mut() {
        for d in s.deps.iter_mut() {
            if let Some(t) = remap.get(d) {
                *d = t.clone();
            }
        }
        s.deps.sort();
        s.deps.dedup();
    }
    *steps = keep;
    removed
}

/// Heavy compatibility preparation is the innermost bound in the validation ladder:
///
/// `420 prep < 480 gate clamp < 600 whole run < 660 local scope < 720 node < 900 job`.
///
/// A 3600s preparation allowance inside a 900s job was unreachable by
/// construction. This bound fires while the scheduler can still name the node
/// and flush its profile row.
const COMPAT_DIAGNOSTIC_WALL_S: i64 = 420;

fn build_release_hermit_node(gate: &str, bin: &str) -> safe_ci_dag_runner::model::Step {
    let default = bin.ends_with("target/release/hermit");
    let cmd = if default {
        "cargo build --release -p hermit --features third-party-backends".to_string()
    } else {
        // A caller-supplied binary is reused rather than rebuilt, but it must
        // exist: silently proceeding with a missing binary would fail every row
        // for a reason that has nothing to do with compatibility.
        format!("test -x {}", validate_plan::shell_quote(bin))
    };
    step_with_caps(
        "compatprep",
        "hermit_release",
        "Release Hermit for compatibility",
        cmd,
        vec![gate.to_string()],
        COMPAT_DIAGNOSTIC_WALL_S,
        COMPAT_DIAGNOSTIC_WALL_S * 2,
        16 * 1024 * 1024 * 1024,
    )
}

fn prepare_fixtures_node(_tag: &str, fixtures: &Path) -> safe_ci_dag_runner::model::Step {
    prepare_fixtures_node_dep(_tag, fixtures, "compatprep.hermit_release")
}

/// The functional-fixture prep node, with an explicit predecessor.
///
/// The `super` suite already builds a release Hermit under its own tag, so it
/// hangs the fixtures off THAT node instead of adding a second identical build.
fn prepare_fixtures_node_dep(
    _tag: &str,
    fixtures: &Path,
    dep: &str,
) -> safe_ci_dag_runner::model::Step {
    step_with_caps(
        "compatprep",
        "fixtures",
        "Functional compatibility fixtures",
        format!(
            "./tests/compat/prepare_real_compat_fixtures.sh {}",
            validate_plan::shell_quote(&fixtures.to_string_lossy())
        ),
        vec![dep.to_string()],
        COMPAT_DIAGNOSTIC_WALL_S,
        COMPAT_DIAGNOSTIC_WALL_S,
        4 * 1024 * 1024 * 1024,
    )
}

/// `require_e9patch_artifacts`' files-only NSS fixture (validate.sh:4095): keeps
/// host identity-daemon races out of the e9patch L2 measurement.
fn nsswitch_fixture_node(path: &Path) -> safe_ci_dag_runner::model::Step {
    let entries = [
        "aliases", "automount", "ethers", "group", "gshadow", "hosts", "initgroups", "netgroup",
        "netmasks", "networks", "passwd", "protocols", "publickey", "rpc", "services", "shadow",
    ]
    .iter()
    .map(|k| format!("{k}: files"))
    .collect::<Vec<_>>()
    .join("\\n");
    step_with_caps(
        "compatprep",
        "nsswitch",
        "e9patch files-only NSS fixture",
        format!(
            "mkdir -p $(dirname {p}) && printf '{entries}\\n' > {p}",
            p = validate_plan::shell_quote(&path.to_string_lossy())
        ),
        vec![],
        60,
        30,
        512 * 1024 * 1024,
    )
}

fn shard_node(gate: &str, lane: &str, nodes: &str) -> safe_ci_dag_runner::model::Step {
    step_with_caps(
        "shard",
        &validate_plan::sanitize_job(&format!("{lane}_{}", nodes.replace([',', '.'], "_"))),
        &format!("DAG shard {lane}:{nodes}"),
        format!(
            "./ci/run-node.sh {} {}",
            validate_plan::shell_quote(lane),
            validate_plan::shell_quote(nodes)
        ),
        vec![gate.to_string()],
        7200,
        7200,
        16 * 1024 * 1024 * 1024,
    )
}

fn step_with_caps(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem: i64,
) -> safe_ci_dag_runner::model::Step {
    safe_ci_dag_runner::model::Step {
        group: group.into(),
        job: job.into(),
        desc: desc.into(),
        description: String::new(),
        cmd,
        deps,
        env: BTreeMap::new(),
        hint: safe_ci_dag_runner::model::ResourceHint {
            rss_baseline_bytes: Some(mem),
            hard_mem_max_bytes: Some(mem),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
    }
}

// --------------------------------------------------------------------------- reporting

/// Per-node cost table, built entirely from typed `StepOutcome` fields.
fn print_cost_table(outcomes: &[StepOutcome], skipped: &[String]) {
    println!("\n=== per-node cost (safe-ci-dag-runner) ===");
    println!("{:<44} {:>9}  {:<8} {}", "node", "seconds", "status", "reason/returncode");
    println!("{}", "-".repeat(84));
    let mut total = 0.0_f64;
    for o in outcomes {
        total += o.duration_s;
        let status = if o.ok {
            "ok"
        } else if o.aborted {
            "ABORTED"
        } else {
            "FAIL"
        };
        let detail = if !o.reason.is_empty() {
            o.reason.clone()
        } else if let Some(rc) = o.returncode {
            if rc < 0 {
                format!("signal {}", -rc)
            } else {
                format!("rc {rc}")
            }
        } else {
            String::new()
        };
        println!("{:<44} {:>9.2}  {:<8} {}", o.tag, o.duration_s, status, detail);
    }
    println!("{}", "-".repeat(84));
    println!("{:<44} {:>9.2}  (sum of node wall)", "TOTAL", total);
    if !skipped.is_empty() {
        println!("\nskipped (dependency failed, never ran): {}", skipped.join(", "));
    }
}

/// Per-program compatibility summary, built from typed node outcomes rather than
/// a scraped TSV. Reproduces `print_compatibility_summary`'s category table.
fn print_compat_summary(mode: CompatMode, outcomes: &[StepOutcome]) -> (usize, usize, Vec<String>) {
    let known = validate_corpus::known_failclosed();
    let diag = validate_corpus::portable_diagnostic();
    let mut per_cat: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut passed = 0usize;
    let mut measured = 0usize;
    let mut blocking_failures: Vec<String> = Vec::new();
    for o in outcomes {
        let Some(label) = o.tag.strip_prefix("compat.") else { continue };
        let cat = validate_corpus::category_of(label);
        let e = per_cat.entry(cat).or_insert((0, 0));
        e.1 += 1;
        measured += 1;
        if o.ok {
            e.0 += 1;
            passed += 1;
            if mode == CompatMode::Strict && known.contains_key(label) {
                println!("  WARN {label} unexpectedly passed fail-closed --strict; drop it from the known-failure table");
            }
        } else if mode == CompatMode::Strict && known.contains_key(label) {
            println!("  WARN {label} known fail-closed under --strict ({}; nonblocking)", known[label]);
        } else if mode == CompatMode::PortableStrict && diag.contains_key(label) {
            println!("  WARN {label} is a bounded portable diagnostic: {}", diag[label]);
        } else {
            blocking_failures.push(label.to_string());
        }
    }
    println!("\nCOMPATIBILITY SUMMARY ({measured} measured programs, mode {})", mode.assurance());
    println!("{:<22} | {:>8} | {:>9}", "Category", "Programs", "passing");
    println!("{}", "-".repeat(46));
    for cat in validate_corpus::CATEGORIES {
        if let Some((p, m)) = per_cat.get(cat) {
            println!("{cat:<22} | {m:>8} | {:>9}", format!("{p}/{m}"));
        }
    }
    println!("{}", "-".repeat(46));
    println!("{:<22} | {measured:>8} | {:>9}", "TOTAL", format!("{passed}/{measured}"));
    println!("P/M means passing/measured; failures are M-P. Unmeasured rows are excluded from M.");
    if mode == CompatMode::Rr {
        // Name the rows deliberately EXCLUDED from the R/R ratchet. A denominator
        // that silently drops five known divergences reads as full coverage.
        let excluded = validate_corpus::rr_known_failures();
        println!(
            "R/R ratchet excludes {} program(s) measured to diverge on replay:",
            excluded.len()
        );
        for (label, why) in &excluded {
            println!("  - {label}: {why}");
        }
    }
    (passed, measured, blocking_failures)
}

/// Conditions that must FAIL a run whatever the ratchet's own arithmetic says,
/// each naming itself so the refusal is readable in the summary.
///
/// The defect this closes, measured 2026-08-08 on `--portable-strict-compat-only`
/// at hermit 0f90722a6: `compatprep.hermit_release` FAILED (it is only
/// `test -x <bin>`), all 188 `compat.*` rows were skipped as dependents, and the
/// run printed `✅ validate PASS (exit 0) — every blocking gate passed` over a
/// `COMPATIBILITY SUMMARY (0 measured programs)`. The cause was structural: for a
/// compat profile the verdict was `effective_failures = compat_blocking` ALONE, so
/// a failure in the build/prep/gate spine — precisely the thing that empties the
/// matrix — contributed nothing, and an empty matrix has no failing rows to count.
/// A ratchet may narrow WHICH measured rows are allowed to fail; it may never
/// decide whether any measurement happened.
///
/// Pure, so `--self-test` can bracket both directions without running a DAG.
fn verdict_refusals(
    compat_measured: Option<usize>,
    structural_failures: usize,
    executed_tests: Option<i64>,
) -> Vec<String> {
    let mut out = Vec::new();
    if structural_failures > 0 {
        out.push(format!(
            "{structural_failures} node(s) OUTSIDE the measured matrix failed; a spine failure \
             empties the matrix and can never be excused by the matrix's own ratchet"
        ));
    }
    // `Some(0)` is a MEASURED zero and is fatal; `None` is unknown and is handled
    // as a NON-VERDICT elsewhere. Conflating the two would turn every profile
    // that reports no count into a red.
    if compat_measured == Some(0) {
        out.push(
            "the compatibility matrix measured ZERO programs; an empty matrix is not a pass"
                .to_string(),
        );
    }
    if executed_tests == Some(0) {
        out.push(
            "ZERO tests executed; a run that executed nothing cannot certify anything".to_string(),
        );
    }
    out
}

/// Two-sided bracket for [`verdict_refusals`]. Inert: no DAG, no ledger, no
/// label, no PR — it exercises the decision function with planted counts only.
fn verdict_refusal_bracket() -> Result<(), String> {
    // POSITIVE 1 — the exact shape measured on 2026-08-08 must fire, and must
    // fire for BOTH reasons rather than collapsing into one.
    let observed = verdict_refusals(Some(0), 1, Some(20));
    if observed.len() != 2 {
        return Err(format!(
            "verdict: the observed fail-open shape (0 measured, 1 spine failure, 20 executed) \
             must trip 2 refusals, tripped {}: {observed:?}",
            observed.len()
        ));
    }
    // POSITIVE 2 — zero executed tests alone, with nothing else wrong.
    if verdict_refusals(None, 0, Some(0)).len() != 1 {
        return Err("verdict: zero executed tests must refuse on its own".into());
    }
    // POSITIVE 3 — a spine failure alone, with a fully measured matrix, still
    // refuses: 187/187 passing rows do not excuse a failed prep node.
    if verdict_refusals(Some(187), 1, Some(862)).len() != 1 {
        return Err("verdict: a spine failure must refuse even with a full matrix".into());
    }
    // NEGATIVE 1 — a genuinely complete run must stay inert, or the gate is a
    // blanket red rather than a predicate.
    let clean = verdict_refusals(Some(187), 0, Some(862));
    if !clean.is_empty() {
        return Err(format!("verdict: a complete run must NOT refuse, got {clean:?}"));
    }
    // NEGATIVE 2 — unknown counts are not a measured zero.
    if !verdict_refusals(None, 0, None).is_empty() {
        return Err("verdict: unknown counts must not be read as a measured zero".into());
    }
    println!(
        "  verdict refusals: 3 positive(s) fire (0-measured+spine, 0-executed, spine-with-full-matrix), \
         2 negative(s) inert (complete run, unknown counts)"
    );
    Ok(())
}

fn human_duration(secs: f64) -> String {
    let x = secs.round() as i64;
    let (h, m, s) = (x / 3600, (x % 3600) / 60, x % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// A positive-integer env override, or `None` when unset/empty/invalid.
fn env_positive(name: &str) -> Option<i64> {
    let v = std::env::var(name).ok()?;
    if v.is_empty() {
        return None;
    }
    match v.parse::<i64>() {
        Ok(n) if n > 0 => Some(n),
        _ => {
            eprintln!("validate: {name}={v:?} is not a positive integer; ignoring");
            None
        }
    }
}

/// Lower every node's wall ceiling to at most `cap`.
fn clamp_wall(plan: &mut Plan, cap: i64) {
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        for s in cfg.steps.iter_mut() {
            s.timeout = s.timeout.min(cap);
        }
    }
}

/// Lower every node's CPU budget to at most `cap`, including the DAG-level
/// default that shipped lane nodes inherit.
fn clamp_cpu(plan: &mut Plan, cap: i64) {
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        cfg.default_step_cpu_timeout = if cfg.default_step_cpu_timeout > 0 {
            cfg.default_step_cpu_timeout.min(cap)
        } else {
            cap
        };
        for s in cfg.steps.iter_mut() {
            s.cpu_timeout = if s.cpu_timeout > 0 { s.cpu_timeout.min(cap) } else { cap };
        }
    }
}

fn propagate_verbosity(plan: &mut Plan, verbosity: i64) {
    let value = verbosity.to_string();
    for step in &mut plan.cfg.steps {
        step.env.insert("VALIDATE_VERBOSITY".into(), value.clone());
    }
    if let Some(second) = &mut plan.second {
        for step in &mut second.steps {
            step.env.insert("VALIDATE_VERBOSITY".into(), value.clone());
        }
    }
}

// --------------------------------------------------------------------------- interruption

/// Set from a signal handler when the operator stops the run.
static INTERRUPTED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_stop_signal(sig: i32) {
    // Async-signal-safe: a relaxed atomic store and nothing else.
    INTERRUPTED.store(sig, std::sync::atomic::Ordering::SeqCst);
}

/// Install SIGINT/SIGTERM/SIGHUP handlers so an operator stop is DISTINGUISHABLE
/// from a run that finished.
///
/// **The ledger records every COMPLETE run — and a timeout IS complete.**
/// A gate that blew its wall or CPU budget produced a real, reproducible result
/// about the tree: it is written, and `timed_out_nodes` says so. An operator
/// pressing Ctrl-C learned nothing about the product, so it is a NO-RESULT and
/// no row is appended at all. Recording interrupts would salt the ledger with
/// rows whose `fail` means "someone stopped it", and every consumer that counts
/// reds — the drain report, the flake classifier, the newest-green frontier —
/// would have to learn to subtract them.
///
/// This is a deliberate change from `validate.sh`, which appended a row with
/// `result: no_result` on a stop. That row was never useful and had to be
/// filtered by every reader; not writing it is strictly simpler.
fn install_stop_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_stop_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_stop_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_stop_signal as *const () as libc::sighandler_t);
    }
}

/// The stopping signal's BARE name (`INT`/`TERM`/`HUP`), not `SIGINT`.
///
/// The bare form is the ledger's `interruption_signal` value and is what
/// `scripts/test_validate_stop_paths.py` asserts
/// (`sig.name.removeprefix("SIG")`). Prose call sites print `SIG{name}`.
fn interrupted_by() -> Option<&'static str> {
    match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        libc::SIGINT => Some("INT"),
        libc::SIGTERM => Some("TERM"),
        libc::SIGHUP => Some("HUP"),
        _ => Some("signal"),
    }
}

// --------------------------------------------------------------- lane execution

/// One lane's terminal state after any environmental retries.
struct LaneResult {
    outcomes: Vec<StepOutcome>,
    skipped: Vec<String>,
    ok: bool,
    /// How many retry ROUNDS this lane needed; recorded in the ledger so a green
    /// that only survived because the host was retried is never mistaken for a
    /// green that passed first time.
    env_retries: usize,
    /// The whole-invocation deadline expired during this lane.
    run_timed_out: bool,
}

/// Read the durable log once it has stopped growing.
///
/// The driver tees its own stdout/stderr through a `tee` child, so a node's
/// `----- detail -----` region reaches the file slightly after the runner emits
/// it. Classifying a failure from a half-written region would misread a genuine
/// product red as "nothing environmental found" — the safe direction, but a
/// silently ineffective mechanism. Flushing and waiting for a stable size makes
/// the read deterministic enough to bind a verdict to.
fn read_log_settled(path: &Path) -> String {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    let size = || std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut last = size();
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let now = size();
        if now > 0 && now == last {
            break;
        }
        last = now;
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Forward nested scheduler rows to the directory uploaded by the hosted shard.
///
/// `validate.rs` invokes the scheduler as a library, bypassing the runner CLI's
/// profile writer. Without this explicit forwarding an inner deadline can name
/// the cut probe on stdout yet leave no per-probe artifact. The workflow uploads
/// `$RUN_NODE_PERF_DIR` under `if: always()`, so these rows survive a red job.
fn forward_step_profiles(result: &RunResult, jobs: i64) {
    let Ok(dir) = std::env::var("RUN_NODE_PERF_DIR") else {
        return;
    };
    if dir.is_empty() || result.step_profile_rows.is_empty() {
        return;
    }
    let git_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    match append_step_profiles(
        Path::new(&dir),
        &result.step_profile_rows,
        &git_sha,
        jobs,
        None,
        "unverified",
        "validate.rs",
    ) {
        Some(path) => eprintln!(
            "validate: wrote {} inner step profile row(s) to {}",
            result.step_profile_rows.len(),
            path.display()
        ),
        None => eprintln!("validate: could not write inner step profile rows to {dir}"),
    }
}

/// Absolute monotonic deadline for one logical invocation.
///
/// A nested validate must spend from the enclosing scheduler step's clock. Starting a new
/// `Instant` after re-exec and setup made `600 < 720` numerically true but temporally false.
fn env_u64(name: &str) -> Result<Option<u64>, String> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(None);
    };
    let text = raw
        .to_str()
        .ok_or_else(|| format!("{name} is not valid UTF-8"))?;
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{name}={text:?} is not an unsigned integer"))
}

fn deadline_from_sources(
    run_timeout_s: Option<i64>,
    nested: bool,
    in_scope: bool,
    step_started_ns: Option<u64>,
    owned_scope_deadline_ns: Option<u64>,
    now_ns: u64,
) -> Result<Option<u64>, String> {
    let Some(timeout_s) = run_timeout_s else {
        return Ok(None);
    };
    let allowance_ns = (timeout_s as u64)
        .checked_mul(1_000_000_000)
        .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))?;
    let scheduler_deadline = match step_started_ns {
        Some(start) if start > now_ns => {
            return Err(format!(
                "scheduler-owned {STEP_STARTED_MONOTONIC_NS_ENV} is in the future"
            ));
        }
        Some(start) => Some(
            start
                .checked_add(allowance_ns)
                .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))?,
        ),
        None => None,
    };
    // Only the top-level same-logical-run re-exec owns this marker. A nested focused payload
    // inherits its parent's scope marker but owns the scheduler epoch for its own enclosing node.
    if in_scope && !nested {
        if let Some(owned) = owned_scope_deadline_ns {
            let latest = now_ns
                .checked_add(allowance_ns)
                .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))?;
            if owned > latest {
                return Err("invocation-owned scope deadline exceeds a fresh full allowance".into());
            }
            if scheduler_deadline.is_some_and(|scheduler| scheduler != owned) {
                return Err("scheduler epoch and invocation-owned scope deadline disagree".into());
            }
            return Ok(Some(owned));
        }
    }
    if let Some(deadline) = scheduler_deadline {
        return Ok(Some(deadline));
    }
    if nested {
        return Err(format!(
            "nested timed validate lacks the scheduler-owned {STEP_STARTED_MONOTONIC_NS_ENV}; \
             refusing to start a fresh clock that could outlive its enclosing node"
        ));
    }
    now_ns
        .checked_add(allowance_ns)
        .map(Some)
        .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))
}

fn invocation_deadline_ns(run_timeout_s: Option<i64>, nested: bool) -> Result<Option<u64>, String> {
    let now_ns = monotonic_now_ns().ok_or_else(|| "CLOCK_MONOTONIC is unavailable".to_string())?;
    deadline_from_sources(
        run_timeout_s,
        nested,
        is_in_scope(),
        env_u64(STEP_STARTED_MONOTONIC_NS_ENV)?,
        env_u64(OWN_SCOPE_DEADLINE_ENV)?,
        now_ns,
    )
}

/// Seconds left on one shared invocation clock, floored so a child cannot outlive it.
fn remaining_budget_s(deadline_ns: Option<u64>) -> Option<i64> {
    let deadline_ns = deadline_ns?;
    // Clock-read failure cannot turn a bounded invocation into `None` (unbounded). Expire it in
    // the safe direction instead.
    let now_ns = monotonic_now_ns().unwrap_or(deadline_ns);
    Some(if now_ns >= deadline_ns {
        0
    } else {
        ((deadline_ns - now_ns) / 1_000_000_000) as i64
    })
}

/// Run one lane, auto-retrying nodes whose failure is an ENVIRONMENTAL block.
///
/// This is `run_check_with_timeout`'s retry loop (validate.sh:2119) moved to DAG
/// granularity. A host FS-permission denial (BPFJailer banner, or a banner-less
/// `EPERM` leaked to `cc1`/`cmake`/`ld`), a `fwdproxy` egress failure, or a
/// vendored third-party (DynamoRIO/elfutils) build flake kills a build or test
/// subprocess for reasons that have nothing to do with the tree under test. Left
/// alone it masquerades as a product failure; the whole point of this loop is
/// that it must not.
///
/// Three properties are preserved from the bash, deliberately:
///
/// * **The classification reads the FAILING NODE's own output**, extracted from
///   the runner's `[tag] ----- detail -----` region, not a whole-log tail. A jail
///   banner printed by a different concurrent node cannot excuse a real red.
/// * **Retries are bounded** (`VALIDATE_ENV_BLOCK_RETRIES`, default 2 => 3
///   attempts). A *persistent* breakage — a bad Reverie pin, a genuinely missing
///   header — fails every attempt and still leaves the run RED. It is never
///   silently greened, only relabelled from "test failure" to "environmental".
/// * **Nodes that never ran because the blocked node failed are retried with
///   it.** In the bash the retry happened INSIDE the gate, so downstream never
///   got skipped; reproducing that here means the retry DAG carries the skipped
///   and aborted nodes too, with dependencies restricted to the retry set.
fn run_lane_with_env_retries(
    cfg: &DagConfig,
    jobs: i64,
    keep_going: bool,
    verbosity: i64,
    cgroups: BoxedCgroups,
    log_path: &Path,
    deadline: Option<u64>,
) -> LaneResult {
    let max = validate_runtime::env_block_max_retries();
    if remaining_budget_s(deadline) == Some(0) {
        eprintln!(
            "validate: whole-run budget expired during setup; no DAG node will be started \
             unbounded, and every planned node is recorded as not attempted"
        );
        return LaneResult {
            outcomes: Vec::new(),
            skipped: cfg.steps.iter().map(|s| s.tag()).collect(),
            ok: false,
            env_retries: 0,
            run_timed_out: true,
        };
    }
    let first = run_dag_boxed_deadline(
        cfg,
        jobs,
        keep_going,
        verbosity,
        cgroups.clone(),
        None,
        None,
        remaining_budget_s(deadline),
    );
    forward_step_profiles(&first, jobs);
    let mut run_timed_out = first.run_timed_out;
    let mut order: Vec<String> = first.outcomes.iter().map(|o| o.tag.clone()).collect();
    let mut by_tag: BTreeMap<String, StepOutcome> =
        first.outcomes.iter().map(|o| (o.tag.clone(), o.clone())).collect();
    let mut skipped = first.skipped.clone();
    let mut env_retries = 0usize;

    while env_retries < max {
        let failed: Vec<&StepOutcome> = by_tag.values().filter(|o| !o.ok && !o.aborted).collect();
        if failed.is_empty() {
            break;
        }
        let log = read_log_settled(log_path);
        if log.is_empty() {
            eprintln!(
                "validate: WARNING: the durable log is unreadable, so {} failed node(s) cannot be \
                 classified as environmental; NOT retrying (an unclassifiable red stays RED).",
                failed.len()
            );
            break;
        }
        let blocked: Vec<(String, &'static str)> = failed
            .iter()
            .filter_map(|o| {
                validate_runtime::extract_node_detail(&log, &o.tag)
                    .and_then(|d| validate_runtime::environmental_block_class(&d))
                    .map(|c| (o.tag.clone(), c))
            })
            .collect();
        if blocked.is_empty() {
            break;
        }
        env_retries += 1;
        for (tag, class) in &blocked {
            println!(
                "⚠️  {tag}: ENVIRONMENTAL block ({class}) — host/sandbox condition, not a test \
                 failure — retrying (attempt {env_retries}/{max})"
            );
        }
        // The retry set: the blocked nodes, plus everything that never ran (or was
        // aborted) because of them.
        let mut keep: BTreeSet<String> = blocked.iter().map(|(t, _)| t.clone()).collect();
        keep.extend(skipped.iter().cloned());
        keep.extend(by_tag.values().filter(|o| o.aborted).map(|o| o.tag.clone()));
        let steps: Vec<safe_ci_dag_runner::model::Step> = cfg
            .steps
            .iter()
            .filter(|s| keep.contains(&s.tag()))
            .map(|s| {
                let mut s = s.clone();
                // Dependencies already satisfied by the first pass are dropped;
                // edges INSIDE the retry set are preserved so a re-run dependency
                // still gates its dependents.
                s.deps.retain(|d| keep.contains(d));
                s
            })
            .collect();
        if steps.is_empty() {
            break;
        }
        let mut retry_cfg = cfg.clone();
        retry_cfg.description =
            format!("{} — environmental retry {env_retries}/{max}", cfg.description);
        retry_cfg.steps = steps;
        // Retries draw down the same clock. Giving every retry a fresh budget
        // would turn a bounded invocation back into an unbounded one.
        if remaining_budget_s(deadline) == Some(0) {
            eprintln!(
                "validate: whole-run budget exhausted; NOT starting environmental retry \
                 {env_retries}/{max}."
            );
            run_timed_out = true;
            break;
        }
        let again = run_dag_boxed_deadline(
            &retry_cfg,
            jobs,
            keep_going,
            verbosity,
            cgroups.clone(),
            None,
            None,
            remaining_budget_s(deadline),
        );
        forward_step_profiles(&again, jobs);
        run_timed_out = run_timed_out || again.run_timed_out;
        for o in &again.outcomes {
            if !by_tag.contains_key(&o.tag) {
                order.push(o.tag.clone());
            }
            by_tag.insert(o.tag.clone(), o.clone());
        }
        skipped = again.skipped.clone();
    }

    // Retries exhausted with an environmental block still standing is a RED, but
    // one whose cause is named. The verdict is unchanged; only its label is.
    if env_retries == max && by_tag.values().any(|o| !o.ok && !o.aborted) {
        let log = read_log_settled(log_path);
        for o in by_tag.values().filter(|o| !o.ok && !o.aborted) {
            if let Some(class) = validate_runtime::extract_node_detail(&log, &o.tag)
                .and_then(|d| validate_runtime::environmental_block_class(&d))
            {
                println!(
                    "🧱 {}: ENVIRONMENTAL BLOCK ({class}) after {} attempt(s) — validate could not \
                     complete this node; this is NOT a test failure, and it is still a RED.",
                    o.tag,
                    max + 1
                );
            }
        }
    }

    let outcomes: Vec<StepOutcome> =
        order.iter().filter_map(|t| by_tag.get(t).cloned()).collect();
    // Eager-exit aborts after a genuine peer failure are neutral, but steps cut
    // by the whole-run clock are not a green. Without the typed run bit here an
    // entirely aborted tail satisfies `ok || aborted` and can falsely pass.
    let ok = !run_timed_out && outcomes.iter().all(|o| o.ok || o.aborted);
    LaneResult { outcomes, skipped, ok, env_retries, run_timed_out }
}

/// Nodes the runner reported as killed by their wall or CPU budget. The runner's
/// own `step_failure_reason` produces these strings, so this reads its typed
/// classification rather than re-deriving one.
fn timed_out_nodes(outcomes: &[StepOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| {
            let r = o.reason.to_ascii_lowercase();
            r.contains("timeout") || r.contains("timed out")
        })
        .map(|o| o.tag.clone())
        .collect()
}

// --------------------------------------------------------------------------- ledger

struct LedgerCtx {
    started_at: String,
    host: String,
    toolchain: String,
    slot: String,
    cwd: String,
    profile: String,
    selection_mode: String,
    cache_state: String,
    commit: String,
    tree: String,
    git_ahead: i64,
    git_behind: i64,
    commit_anchored: bool,
    tree_dirty: bool,
    dag_jobs: i64,
    /// Only the canonical validate-lock owner ancestry establishes admission.
    admission: Option<&'static str>,
    /// Exact base identities from the parent's single receipt finalizer. Each
    /// stays null when that proof cannot be computed.
    base_sha: serde_json::Value,
    base_tree: serde_json::Value,
    reverie_base_sha: serde_json::Value,
    reverie_base_tree: serde_json::Value,
    /// Peak number of OTHER top-level validates that were provably live AND
    /// burning CPU beside this run. `None` means UNKNOWN (never 0-by-default): a
    /// bare run with no registry is not proven exclusive.
    concurrent_validates: Option<i64>,
    /// How that number was established, so a reader never has to guess whether a
    /// `0` is "measured exclusive" or "nobody looked".
    concurrency_proof: Option<&'static str>,
    /// `INT` / `TERM` / `HUP` when an operator stopped the run.
    interruption: Option<String>,
    /// Whole-run CPU seconds (self + reaped children), the same pair printed in
    /// the summary line.
    cpu_user: f64,
    cpu_sys: f64,
    /// Retry ROUNDS spent on environmental blocks; `0` for a clean first pass.
    env_block_retries: i64,
    /// Whether THIS run observed the `pre.reverie_pin` gate pass. Recorded on the
    /// row itself so a reader never has to infer from a bare `pass` that the
    /// archival pin was proved current; the receipt verifier keys on it.
    reverie_pin_current: bool,
    /// libtest counts parsed from the durable log; `None` is UNKNOWN.
    executed_tests: Option<i64>,
    filtered_tests: Option<i64>,
}

struct ReceiptEvidence {
    coverage: serde_json::Value,
    base_sha: serde_json::Value,
    base_tree: serde_json::Value,
    reverie_base_sha: serde_json::Value,
    reverie_base_tree: serde_json::Value,
}

impl Default for ReceiptEvidence {
    fn default() -> Self {
        Self {
            coverage: serde_json::Value::Null,
            base_sha: serde_json::Value::Null,
            base_tree: serde_json::Value::Null,
            reverie_base_sha: serde_json::Value::Null,
            reverie_base_tree: serde_json::Value::Null,
        }
    }
}

/// Ask the parent's single receipt finalizer for coverage and base identities.
/// Any missing helper, failed command, or malformed output stays explicit null;
/// the schema-5 consumer then refuses qualification.
fn receipt_evidence(
    parent: Option<&Path>,
    root: &Path,
    log: &Path,
    commit: &str,
) -> ReceiptEvidence {
    let Some(parent) = parent else { return ReceiptEvidence::default() };
    let helper = parent.join("ci-hub/validate/finalize_receipt.py");
    if !helper.is_file() || log.as_os_str().is_empty() || commit.is_empty() {
        return ReceiptEvidence::default();
    }
    let Ok(out) = Command::new("python3")
        .arg(&helper)
        .arg("--log")
        .arg(log)
        .arg("--sha")
        .arg(commit)
        .arg("--hermit-checkout")
        .arg(root)
        .arg("--emit-only")
        .output()
    else {
        return ReceiptEvidence::default();
    };
    if !out.status.success() {
        return ReceiptEvidence::default();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return ReceiptEvidence::default();
    };
    let coverage = value.get("coverage").cloned().unwrap_or(serde_json::Value::Null);
    let (_, coverage) = ledger_schema_and_coverage(coverage);
    let field = |name: &str| value.get(name).cloned().unwrap_or(serde_json::Value::Null);
    ReceiptEvidence {
        coverage,
        base_sha: field("base_sha"),
        base_tree: field("base_tree"),
        reverie_base_sha: field("reverie_base_sha"),
        reverie_base_tree: field("reverie_base_tree"),
    }
}

/// Ask the canonical parent lock authority whether this exact run is admitted.
/// Production never trusts caller-supplied owner PIDs or sidecar paths. The
/// stop-test JSON seam is confined to an intrinsically non-qualifying fixture.
fn canonical_validate_lock_admission(
    parent: Option<&Path>,
    commit: &str,
    host: &str,
) -> bool {
    fn object_string<'a>(
        object: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<&'a str> {
        object.get(key).and_then(serde_json::Value::as_str)
    }
    let status = if env_flag("HERMIT_VALIDATE_STOP_TEST_MODE", "1") {
        let Ok(fixture) = std::env::var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON") else {
            return false;
        };
        fixture.into_bytes()
    } else {
        let Some(parent) = parent else { return false };
        let ci_hub = parent.join("ci-hub/ci-hub");
        if !ci_hub.is_file() {
            return false;
        }
        let Ok(output) = Command::new(ci_hub)
            .args(["validate-lock", "authority-status", "--json"])
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        output.stdout
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&status) else {
        return false;
    };
    let Some(holder) = value.get("holder").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(owner) = value.get("owner").and_then(serde_json::Value::as_object) else {
        return false;
    };
    if value.get("schema_version").and_then(serde_json::Value::as_i64) != Some(1)
        || value.get("admissible").and_then(serde_json::Value::as_bool) != Some(true)
        || value.get("state").and_then(serde_json::Value::as_str) != Some("held")
        || !value.get("reason_code").is_some_and(serde_json::Value::is_null)
        || value.get("canonical_anchor_held").and_then(serde_json::Value::as_bool) != Some(true)
        || !matches!(
            value.get("cleanup_state").and_then(serde_json::Value::as_str),
            Some("none" | "active-bound")
        )
        || object_string(holder, "kind") != Some("validate")
        || object_string(holder, "target") != Some(commit)
        || object_string(holder, "host") != Some(host)
        || object_string(owner, "host") != Some(host)
        || object_string(owner, "liveness") != Some("alive")
    {
        return false;
    }
    let Some(pid64) = owner.get("pid").and_then(serde_json::Value::as_i64) else {
        return false;
    };
    let Some(start_ticks) = owner.get("start_ticks").and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Ok(pid) = i32::try_from(pid64) else { return false };
    if pid <= 1 || start_ticks == 0 {
        return false;
    }
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string());
    if boot_id.as_deref() != object_string(owner, "boot_id") {
        return false;
    }
    validate_runtime::identity_in_ancestry(pid, start_ticks)
}

/// Parse the libtest `executed` / `filtered` counts out of the durable log.
///
/// **This is the field the whole receipt rests on.** A row whose
/// `executed_tests` is null is a NON-VERDICT: every downstream completeness
/// predicate keys `is_clean_full_pass` on a nonzero executed count, so a driver
/// that ran no tests at all would otherwise emit a row indistinguishable from one
/// that ran the whole suite. `main` at `61edbef4` recorded 862 executed / 693
/// filtered, and a port that cannot reproduce that number has not preserved the
/// thing validate exists to do.
///
/// Deliberately NOT re-implemented here: the banner parser lives once, in the
/// parent (`ci-hub/remediation/nonzero_result.py --ledger-fields`), and every
/// consumer calls that one. A second in-tree parser would be a second authority
/// that can disagree. A missing helper, an unreadable log, or unparseable output
/// all yield `None` (UNKNOWN) — never a fabricated zero.
fn libtest_counts(parent: Option<&Path>, log: &Path) -> (Option<i64>, Option<i64>) {
    let Some(parent) = parent else { return (None, None) };
    let helper = parent.join("ci-hub/remediation/nonzero_result.py");
    if !helper.is_file() || log.as_os_str().is_empty() {
        return (None, None);
    }
    let Ok(out) = Command::new("python3")
        .arg(&helper)
        .arg("--ledger-fields")
        .arg(log)
        .output()
    else {
        return (None, None);
    };
    if !out.status.success() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let parse = |v: Option<&str>| v.and_then(|v| v.parse::<i64>().ok());
    (parse(it.next()), parse(it.next()))
}

/// Write one validation record through the single configured authority.
///
/// Every qualification is written HERE, at the single write point, so no
/// downstream reader can pair a bare `pass` with inferred coverage. Field names
/// and schema match what `validate.sh` wrote, so the parent aggregator and the
/// merge gate keep reading one shape across the port.
#[allow(clippy::too_many_arguments)]
fn write_ledger(
    ledger: &Path,
    ctx: &LedgerCtx,
    outcomes: &[StepOutcome],
    skipped: &[String],
    wall_s: f64,
    exit_code: u8,
    log_file: &str,
    suite_complete: bool,
    coverage: serde_json::Value,
) {
    let (ledger_schema, coverage) = ledger_schema_and_coverage(coverage);
    let gates_run = outcomes.len();
    let failures = outcomes.iter().filter(|o| !o.ok && !o.aborted).count();
    // An operator stop learned nothing new about the product. Preserve the raw
    // shell outcome for forensics, but do not mint a FAILED verdict unless a
    // completed gate had already established one before the stop
    // (validate.sh:1473 `interruption_is_no_result`).
    let raw_result = if exit_code == 0 && failures == 0 { "pass" } else { "fail" };
    let result =
        if ctx.interruption.is_some() && failures == 0 { "no_result" } else { raw_result };
    let timed_out = timed_out_nodes(outcomes);
    // Stable per-row identity. Corrections never edit a row; they append a new
    // one carrying `corrects: <this id>`, which is what keeps the shard
    // append-only and safe to union across machines.
    let record_id = format!("{}-{}-{}", ctx.host, epoch_now(), std::process::id());
    let gates_expected = if ctx.profile == "full" && suite_complete {
        serde_json::json!(gates_run)
    } else {
        serde_json::Value::Null
    };
    let gates: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|o| {
            serde_json::json!({
                "name": o.tag,
                "result": if o.ok { "pass" } else { "fail" },
                "exit_code": o.returncode,
                "reason": o.reason,
                "aborted": o.aborted,
                "real_seconds": o.duration_s,
            })
        })
        .collect();
    let record = serde_json::json!({
        "schema_version": ledger_schema,
        "repo": "hermit",
        "producer": LEDGER_PRODUCER,
        "admission": ctx.admission,
        // Immutable-row identity. `corrects` is null here; a correcting row
        // repeats this shape with `corrects` set to the id it supersedes.
        "record_id": record_id,
        "corrects": serde_json::Value::Null,
        "started_at": ctx.started_at,
        "finished_at": utc_now(),
        "host": ctx.host,
        "toolchain": ctx.toolchain,
        "slot": ctx.slot,
        "cwd": ctx.cwd,
        "profile": ctx.profile,
        "selection_mode": ctx.selection_mode,
        "cache_state": ctx.cache_state,
        "commit": ctx.commit,
        "tree": ctx.tree,
        "git_ahead": ctx.git_ahead,
        "git_behind": ctx.git_behind,
        "commit_anchored": ctx.commit_anchored,
        "tree_dirty": ctx.tree_dirty,
        "base_sha": ctx.base_sha,
        "base_tree": ctx.base_tree,
        "reverie_base_sha": ctx.reverie_base_sha,
        "reverie_base_tree": ctx.reverie_base_tree,
        "reverie_pin_current": ctx.reverie_pin_current,
        "result": result,
        "raw_result": raw_result,
        "exit_code": exit_code,
        "checks": gates_run,
        "failures": failures,
        "dag_jobs": ctx.dag_jobs,
        // Peak CPU-ACTIVE peer validates, and HOW that was established. `null`
        // means UNKNOWN — a bare run with no observed peer is not proven
        // exclusive, and writing 0 there would be a fabricated exclusivity claim.
        "concurrent_validates": ctx.concurrent_validates,
        "concurrency_proof": ctx.concurrency_proof,
        // Present (non-null) only for an operator stop; `result` above is then
        // `no_result` unless a completed gate had already failed.
        "interruption_signal": ctx.interruption,
        // Whole-run CPU (self + reaped children), the same numbers the printed
        // summary carries. Wall alone cannot separate a busy run from a wedged
        // one; the pair can.
        "user_seconds": ctx.cpu_user,
        "sys_seconds": ctx.cpu_sys,
        // Retry ROUNDS spent on environmental blocks. A green that only survived
        // because the host was retried must be distinguishable from a first-pass
        // green.
        "env_block_retries": ctx.env_block_retries,
        // LIBTEST counts parsed from the durable log by the parent's single-
        // sourced banner parser, exactly as validate.sh:1671 recorded them.
        // `null` is UNKNOWN and stays UNKNOWN: the receipt publisher fails closed
        // rather than turning missing evidence into a zero or a pass. These are
        // the counts every downstream `is_clean_full_pass` predicate keys on, so
        // a row without them is a NON-VERDICT, not a green.
        "executed_tests": ctx.executed_tests,
        "filtered_tests": ctx.filtered_tests,
        "gates_run": gates_run,
        "gates_expected": gates_expected,
        "skipped_nodes": skipped.len(),
        // A timeout is a RESULT, so it is recorded rather than dropped, and it is
        // named so a reader can separate "the tree is broken" from "a gate blew
        // its budget". Operator interrupts never reach this function at all.
        "timed_out_nodes": timed_out,
        // NODE counts, deliberately NOT named executed_tests/filtered_tests: a
        // schema<5 consumer keys is_clean_full_pass on those libtest-count names,
        // and a ~47-NODE DAG run must never be readable as a 47-TEST pass. The
        // counted receipt is minted by finalize_receipt.py --scan off the log.
        "executed_nodes": gates_run,
        "real_seconds": wall_s,
        "log_file": log_file,
        "coverage": coverage,
        "gates": gates,
    });
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    let explicit = std::env::var(LEDGER_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .is_some_and(|value| Path::new(&value) == ledger);
    if !explicit && ledger.file_name().is_some_and(|name| name == "ledger") {
        let Some(parent) = ledger.parent() else {
            eprintln!("validate: warning: canonical ledger root has no parent: {}", ledger.display());
            return;
        };
        let adapter = parent.join("ci-hub/ledger/validate_rows.py");
        let mut child = match Command::new("python3")
            .arg(&adapter)
            .arg("record")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!(
                    "validate: warning: cannot launch canonical ledger writer {}: {e}",
                    adapter.display()
                );
                return;
            }
        };
        use std::io::Write;
        let write_error = child
            .stdin
            .take()
            .and_then(|mut stdin| stdin.write_all(line.as_bytes()).err());
        let output = child.wait_with_output();
        if let Some(error) = write_error {
            eprintln!("validate: warning: cannot send row to canonical ledger writer: {error}");
            return;
        }
        match output {
            Ok(output) if output.status.success() => eprintln!(
                "validate: canonical ledger record appended via {}: {}",
                adapter.display(),
                String::from_utf8_lossy(&output.stdout).trim()
            ),
            Ok(output) => eprintln!(
                "validate: warning: canonical ledger writer {} refused: {}",
                adapter.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(e) => eprintln!(
                "validate: warning: cannot wait for canonical ledger writer {}: {e}",
                adapter.display()
            ),
        }
        return;
    }

    if let Some(dir) = ledger.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("validate: warning: cannot create ledger dir {}: {e}", dir.display());
                return;
            }
        }
    }
    use std::io::Write;
    match std::fs::OpenOptions::new().create(true).append(true).open(ledger) {
        Ok(mut f) => match f.write_all(line.as_bytes()) {
            Ok(()) => {
                eprintln!(
                    "validate: fixture/standalone ledger record appended to {}",
                    ledger.display()
                );
                warn_if_unreadable_ledger(ledger);
            }
            Err(e) => eprintln!("validate: warning: cannot append ledger {}: {e}", ledger.display()),
        },
        Err(e) => eprintln!("validate: warning: cannot open ledger {}: {e}", ledger.display()),
    }
}

/// SHORT hostname, never an FQDN.
///
/// The shard name is part of a committed path, and an FQDN would leak internal
/// domain structure into the repository as well as making the same machine
/// produce different shard names depending on how DNS resolved that day. `hostname
/// -s` is the short form; anything with a dot is truncated at the first label as a
/// belt-and-braces guard in case `-s` is unavailable.
fn short_hostname() -> String {
    let raw = sh("hostname", &["-s"])
        .or_else(|| sh("hostname", &[]))
        .unwrap_or_else(|| "unknown".into());
    raw.split('.').next().unwrap_or("unknown").to_string()
}

/// Resolve the logical ledger authority. Precedence:
///   1. `$HERMIT_VALIDATE_LEDGER` — explicit fixture/standalone file.
///   2. `$DEV_HERMIT_PARENT/ledger` — the canonical adapter-backed union.
///   3. A discovered dev-hermit parent's canonical union.
///   4. The standalone in-repo diagnostic shard.
fn ledger_path(root: &Path) -> PathBuf {
    if let Ok(explicit) = std::env::var(LEDGER_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    if let Ok(parent) = std::env::var(PARENT_ENV) {
        if !parent.is_empty() {
            return PathBuf::from(parent).join("ledger");
        }
    }
    let team = std::env::var(LEDGER_TEAM_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| LEDGER_TEAM_DEFAULT.to_string());
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
            .collect::<String>()
    };
    // CONFLICT RESOLUTION (rebase onto cd428f96): main added this parent-discovery step and this
    // PR replaced the fallback beneath it. Both are kept -- the discovery runs FIRST, then this
    // PR's team/host fallback. Dropping it would have silently reverted a landed fix.
    // main's rationale, preserved verbatim: the env var being unset does NOT mean there is no
    // parent -- far more often it means a run inside a dev-hermit slot that simply did not export
    // it. Measured 2026-08-08: 111 real rows sat in two slots' local ledgers for exactly that
    // reason, and `ci-hub validate-status` could not see one of them.
    if let Some(found) = discover_parent_ledger(root) {
        eprintln!(
            "validate.rs: {PARENT_ENV} is unset; recording to the DISCOVERED parent ledger {}",
            found.display()
        );
        return found;
    }
    root.join(LEDGER_DIR)
        .join(format!("{}.{}.jsonl", sanitize(&team), sanitize(&short_hostname())))
}

/// Walk up from `root` for the dev-hermit parent that owns the canonical adapter.
///
/// Deliberately keyed on the executable contract, not a directory name or a
/// retired raw file. Returns `None` only for a genuinely standalone checkout.
fn discover_parent_ledger(root: &Path) -> Option<PathBuf> {
    let mut dir = root.parent();
    while let Some(candidate) = dir {
        let adapter = candidate.join("ci-hub/ledger/validate_rows.py");
        if adapter.is_file() {
            return Some(candidate.join("ledger"));
        }
        dir = candidate.parent();
    }
    None
}

/// Say plainly that a row is not going anywhere a reader will look.
///
/// A writer that SUCCEEDS into a location no consumer reads reports success and attests nothing --
/// the same shape as a `locally-validated` label with no backing run. This does not fail the run,
/// because a standalone checkout must still be able to validate; it makes the invisibility
/// impossible to miss, so "silent success" stops being the failure mode.
///
/// CONFLICT RESOLUTION: main keyed this on `LOCAL_LEDGER_BASENAME`, which this PR removes. Re-keyed
/// to this PR's `LEDGER_DIR` fallback, which is the same thing under the new design -- the location
/// no reader queries. Behaviour preserved, constant adapted.
fn warn_if_unreadable_ledger(ledger: &Path) {
    if !ledger.parent().is_some_and(|p| p.ends_with(LEDGER_DIR)) {
        return;
    }
    eprintln!(
        "validate.rs: WARNING: this row is going to the CHECKOUT-LOCAL ledger {}, which NO reader \
         queries -- `ci-hub validate-status` will report NOT-VALIDATED for this commit even though \
         the run passed. Set {PARENT_ENV} to the dev-hermit workspace (or {LEDGER_ENV} to an \
         explicit file) if this row is meant to count.",
        ledger.display()
    );
}

// --------------------------------------------------------------------------- main

// --------------------------------------------------------------------- summary

/// What the invocation concluded. One variant per way validate can stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Pass,
    Fail,
    /// Admission control declined to run: dirty tree, stale base, unplannable
    /// profile, uncapped node, no boxing, no durable log, bad arguments.
    Refused,
    /// An operator stop. A NO-RESULT, not a failure.
    Interrupted,
    /// `--show-plan`: nothing was executed by design.
    PlanOnly,
    /// A prior passing record for this exact tree was reused.
    CacheHit,
    SelfTest,
    /// `--help`; the usage text IS the output.
    Help,
}

impl Verdict {
    fn marker(self) -> &'static str {
        match self {
            Verdict::Pass | Verdict::SelfTest | Verdict::CacheHit => "✅",
            Verdict::Fail => "❌",
            Verdict::Refused => "🚫",
            Verdict::Interrupted => "⏹",
            Verdict::PlanOnly => "📋",
            Verdict::Help => "",
        }
    }
    fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Refused => "REFUSED",
            Verdict::Interrupted => "INTERRUPTED (no result)",
            Verdict::PlanOnly => "PLAN ONLY (nothing executed)",
            Verdict::CacheHit => "PASS (cache hit; nothing executed)",
            Verdict::SelfTest => "SELF-TEST",
            Verdict::Help => "HELP",
        }
    }
}

/// The end-of-run summary. **Every** exit path constructs one.
///
/// Owner directive (2026-08-07): "Validate itself should ALWAYS print a SUMMARY
/// at the end." That is enforced STRUCTURALLY rather than by discipline: `run`
/// returns a `RunSummary` instead of an exit code, so a new early return cannot
/// compile without saying what it concluded, and `main` is the single place that
/// renders it. A scope guard would have been weaker — it can only print a
/// default, whereas this makes each path state WHAT was refused and WHY.
///
/// The renderer runs BEFORE `DurableLog::finish`, so the summary is written into
/// the durable log as well as the terminal. The motivating gap was a real run
/// (main tip d2cdd2317, slot sol-validate, 2026-08-07T16:37:51Z) whose log ended
/// with a bare `Exit: 1 / Duration: 0s` and no conclusion at all.
struct RunSummary {
    verdict: Verdict,
    exit_code: u8,
    /// One or more lines naming what happened; for a refusal, what and why.
    detail: Vec<String>,
    profile: String,
    commit: String,
    nodes_executed: usize,
    nodes_failed: usize,
    nodes_skipped: usize,
    wall_s: Option<f64>,
    jobs: Option<i64>,
    log: Option<PathBuf>,
    ledger: Option<PathBuf>,
    /// `(wall, user, sys)` seconds for the WHOLE invocation, measured once at the
    /// single cleanup point so the ledger row and the printed summary carry
    /// byte-identical numbers (validate.sh:1855 made the same guarantee, and for
    /// the same reason: two independently-sampled "totals" that disagree make the
    /// receipt unciteable). `None` on a path that stopped before cleanup; `main`
    /// then measures live rather than printing nothing.
    cpu_wall: Option<(f64, f64, f64)>,
}

impl RunSummary {
    fn new(verdict: Verdict, exit_code: u8, profile: &str, detail: Vec<String>) -> Self {
        RunSummary {
            verdict,
            exit_code,
            detail,
            profile: profile.to_string(),
            commit: git_sha(),
            nodes_executed: 0,
            nodes_failed: 0,
            nodes_skipped: 0,
            wall_s: None,
            jobs: None,
            log: None,
            ledger: None,
            cpu_wall: None,
        }
    }
    /// Admission control declined. `what` names the gate, `why` the reason.
    fn refused(exit_code: u8, profile: &str, what: &str, why: Vec<String>) -> Self {
        let mut detail = vec![format!("refused by: {what}")];
        detail.extend(why);
        RunSummary::new(Verdict::Refused, exit_code, profile, detail)
    }
}

/// The ONE summary renderer. Called from exactly one place.
///
/// `started` is the process's own start instant, used only when a path stopped
/// before cleanup could take the authoritative measurement.
fn print_run_summary(s: &RunSummary, started: std::time::Instant) {
    if s.verdict == Verdict::Help {
        return;
    }
    println!();
    println!(
        "{} validate {} (exit {}) — profile {} @ {}",
        s.verdict.marker(),
        s.verdict.word(),
        s.exit_code,
        s.profile,
        s.commit
    );
    for line in &s.detail {
        println!("   {line}");
    }
    // Node accounting is printed whenever a DAG ran, and deliberately printed as
    // an explicit zero when one did not, so "no nodes ran" is a stated fact
    // rather than an absent line a reader has to interpret.
    match s.wall_s {
        Some(wall) => println!(
            "   nodes: {} executed, {} failed, {} skipped in {}{}",
            s.nodes_executed,
            s.nodes_failed,
            s.nodes_skipped,
            human_duration(wall),
            s.jobs.map(|j| format!(" at -j {j}")).unwrap_or_default()
        ),
        None => println!("   nodes: none executed (stopped before the DAG ran)"),
    }
    match &s.log {
        Some(p) => println!("   durable log: {}", p.display()),
        None => println!("   durable log: (none — stopped before one was opened)"),
    }
    if let Some(p) = &s.ledger {
        println!("   ledger: {}", p.display());
    }
    // ALWAYS printed, on success, failure, refusal, timeout and interruption
    // alike (validate.sh:1751). Wall alone cannot tell a busy run from a wedged
    // one; CPU (user+sys, this process plus every child it reaped) against wall
    // can, and that ratio is how the 53-minute pre-gate wedge was identified on
    // 2026-08-07 — the wall clock said "still going", the ratio said "waiting".
    let (wall, user, sys) = s
        .cpu_wall
        .unwrap_or_else(|| {
            let (u, sy) = validate_runtime::process_cpu_seconds();
            (started.elapsed().as_secs_f64(), u, sy)
        });
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!(
        "   {}",
        validate_runtime::cpu_wall_line(human_duration, wall, user, sys, host_cpus)
    );
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    install_stop_handlers();
    let started = std::time::Instant::now();

    // The durable log outlives `run` so the summary lands INSIDE it.
    let mut durable: Option<DurableLog> = None;
    let summary = run(&mut durable);
    print_run_summary(&summary, started);
    if let Some(d) = durable.take() {
        d.finish();
    }
    ExitCode::from(summary.exit_code)
}

/// The whole invocation, returning what it concluded rather than an exit code.
fn run(durable_slot: &mut Option<DurableLog>) -> RunSummary {
    let args = match parse_args() {
        Ok(a) => a,
        // `parse_args` returns 0 only for `--help`, whose usage text is the
        // output; anything else is a genuine CLI refusal and gets a summary.
        Err(0) => return RunSummary::new(Verdict::Help, 0, "help", vec![]),
        Err(code) => {
            return RunSummary::refused(
                code,
                "(arguments not parsed)",
                "argument parsing",
                vec!["see the message above; run --help for the accepted flags".into()],
            )
        }
    };

    if args.self_test {
        return match self_test() {
            Ok(()) => RunSummary::new(
                Verdict::SelfTest,
                0,
                "self-test",
                vec![
                    "force-full policy brackets, shell quoting, corpus counts, super gate table, \
                     envelope scoring/comparison, ledger cache, receipt eligibility, and the \
                     selective subset builder all passed"
                        .into(),
                    "every bracket is inert: none runs a gate, publishes a label, or writes the \
                     real ledger"
                        .into(),
                ],
            ),
            Err(e) => {
                eprintln!("validate: SELF-TEST FAILED: {e}");
                RunSummary::new(Verdict::Fail, 2, "self-test", vec![format!("self-test failed: {e}")])
            }
        };
    }

    let level_name = args.level.name().to_string();
    let root = repo_root();
    if std::env::set_current_dir(&root).is_err() {
        return RunSummary::refused(
            2,
            &level_name,
            "repository root",
            vec![format!("cannot cd to repo root {}", root.display())],
        );
    }
    let parent = find_parent(&root);
    // The profile name is needed by the admission gates below, which run BEFORE
    // the plan exists. It is derived exactly as `build_plan` derives it, so the
    // lock record and the ledger row can never disagree about what was running.
    let profile_name =
        args.focused.as_ref().map(|f| f.profile()).unwrap_or_else(|| level_name.clone());

    // ---- re-entrancy (validate.sh:460) ---------------------------------------
    //
    // `ci/dag/portable.json`'s `test.strict_compat` node runs
    // `./scripts/validate.rs --portable-strict-compat-only`, so re-entry is a DESIGNED
    // path. What must never happen is a full driver inside a full driver: it pays
    // the whole preamble twice, appends a SECOND ledger row, and can publish a
    // SECOND receipt for one logical run. A nested FOCUSED invocation is a
    // PAYLOAD — the outer run owns the ledger, receipt, cache, lock and
    // concurrency accounting; a nested non-focused level is refused outright.
    let nesting = validate_runtime::detect_nesting();
    if let Some(stale) = nesting.stale_marker {
        eprintln!(
            "validate: ignoring a STALE {} marker naming pid {stale}: that pid is not an ancestor \
             of this process, so this is a TOP-LEVEL run. (Treating the bare env var as proof of \
             nesting would refuse every legitimate full run in a shell that once exported it.)",
            validate_runtime::ACTIVE_ENV
        );
    }
    // The marker is claimed LATER, after the cgroup re-exec -- see the call site
    // below resolve_cgroups. Claiming it here made the driver REFUSE ITSELF:
    // resolve_cgroups re-execs into a transient systemd scope for boxing, the
    // re-exec inherits the environment, and the new process is a genuine
    // DESCENDANT of the claimer -- so is_ancestor() was true and it read its own
    // boxing re-exec as a nested run. Measured: a full profile could not start at
    // all under boxing, refusing with "outer pid <the scope's own parent>" in 0s.
    // --self-test and --show-plan both missed it because neither re-execs.
    //
    // The boxing re-exec is the SAME logical run, not a nested one. Only the
    // process that survives the re-exec should claim the marker.
    if nesting.nested && args.focused.is_none() {
        let outer = nesting.outer_pid.unwrap_or(-1);
        eprintln!(
            "validate: refusing to re-enter a full validation level from inside validate (outer \
             pid {outer}); nested invocations may only run a focused mode."
        );
        return RunSummary::refused(
            2,
            &profile_name,
            "the re-entrancy guard",
            vec![
                format!("this process is a descendant of validate pid {outer}, which is already driving a run"),
                "a full suite inside a full suite would pay the whole preamble twice, append a \
                 SECOND ledger row, and could publish a SECOND receipt for one logical run"
                    .into(),
                "nested invocations may run ONE focused mode as a payload; the outer run owns the \
                 ledger, receipt, cache and concurrency accounting"
                    .into(),
            ],
        );
    }

    // ---- stop-path test seam (validate.sh:1899) ------------------------------
    //
    // Placed before every admission gate on purpose: this fixture exists to
    // exercise the REAL signal traps and the REAL ledger writer without starting
    // a product build, so making it depend on the checkout's cleanliness or
    // freshness would turn `scripts/test_validate_stop_paths.py` into a test of
    // this tree's state instead of the stop paths. It deliberately does NOT take
    // the invocation lock: it never runs a gate, and a leaked fixture must never
    // wedge a real run.
    if validate_runtime::stop_test_requested() {
        return stop_test_seam(&root, &profile_name, parent.as_deref());
    }

    // Anchor the logical run before locks, freshness checks, plan construction, cgroup re-exec,
    // durable-log setup, and registration.  A nested focused payload inherits the enclosing
    // safe-ci step's scheduler-owned epoch; a top-level run owns its epoch here.
    let run_timeout = args
        .run_timeout
        .or_else(|| env_positive("HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS"));
    let deadline_ns = if args.show_plan {
        None
    } else {
        match invocation_deadline_ns(run_timeout, nesting.nested) {
            Ok(deadline) => deadline,
            Err(msg) => {
                eprintln!("validate: REFUSED — {msg}");
                return RunSummary::refused(
                    3,
                    &profile_name,
                    "the shared timeout epoch",
                    vec![msg],
                );
            }
        }
    };

    // ---- concurrent invocation (validate.sh:492) -----------------------------
    //
    // A second validate in the SAME checkout is unambiguously wrong: both drive
    // one `target/` tree and one ledger. Refuse LOUDLY and IMMEDIATELY, naming
    // the holder — never wait, and never let two interleave. Scope is
    // PER-CHECKOUT; box-wide exclusivity belongs to `ci-hub validate-lock`, and
    // duplicating it here would give the fleet two admission controllers that can
    // disagree. `--show-plan` executes nothing, so it is not a second driver and
    // does not contend.
    let _invocation_lock;
    if !nesting.nested && !args.show_plan {
        match validate_runtime::acquire_invocation_lock(&root, &profile_name, &git_sha()) {
            validate_runtime::LockOutcome::Acquired(l) => _invocation_lock = Some(l),
            validate_runtime::LockOutcome::Busy(why) => {
                eprintln!("validate: REFUSED — {}", why[0]);
                for line in why.iter().skip(1) {
                    eprintln!("  {line}");
                }
                return RunSummary::refused(3, &profile_name, "the per-checkout invocation lock", why);
            }
            validate_runtime::LockOutcome::Unavailable(e) => {
                // Fail OPEN here, deliberately: refusing every run because a lock
                // file could not be created would be a larger outage than the
                // concurrency it guards, and the condition is stated rather than
                // swallowed.
                eprintln!("validate: WARNING: per-checkout invocation lock unavailable ({e}); proceeding UNGUARDED.");
                _invocation_lock = None;
            }
        }
    } else {
        _invocation_lock = None;
    }

    // Dirty-tree gate, BEFORE any state is created, so a refusal leaves nothing
    // behind. A result validated against uncommitted changes describes a tree
    // that exists nowhere in history and cannot be reproduced or compared.
    // Skipped for a nested payload: the outer run already made this judgement
    // about the same checkout, and a second answer could only disagree.
    let wt_dirty = worktree_dirty();
    if !nesting.nested && wt_dirty && !args.run_on_dirty_tree {
        eprintln!("validate: refusing to run on a dirty working tree.");
        eprintln!("  HEAD {} has uncommitted working-tree changes, so a record anchored to it", git_sha());
        eprintln!("  would describe a tree that exists nowhere in history. Commit (preferred), or");
        eprintln!("  stage the WIP with 'git add', then re-run. To force an explicitly unanchored");
        eprintln!("  run pass --run-on-dirty-tree (agents must not).");
        let _ = Command::new("git").args(["status", "--short"]).status();
        return RunSummary::refused(
            2,
            &level_name,
            "the dirty-working-tree gate",
            vec![
                "HEAD has uncommitted working-tree changes, so a record anchored to it would \
                 describe a tree that exists nowhere in history"
                    .into(),
                "commit (preferred) or `git add` the WIP, then re-run; --run-on-dirty-tree forces \
                 an explicitly unanchored run"
                    .into(),
            ],
        );
    }

    // Rebase-freshness gate. Mechanically enforced, not advisory. A nested
    // payload inherits the outer run's verdict on the very same checkout; it also
    // must not spend a network round trip inside a budgeted DAG node.
    match rebase_freshness(args.run_on_dirty_tree || nesting.nested) {
        Ok(msg) => eprintln!("validate: {msg}"),
        Err(msg) => {
            eprintln!("validate: refusing to validate a stale base.\n  {msg}");
            return RunSummary::refused(
                2,
                &level_name,
                "the rebase-freshness gate",
                msg.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
            );
        }
    }

    // Run state lives under target/, never under HERMIT_DIR (a user setting).
    let tmp = root.join("target/validation").join(format!("run-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return RunSummary::refused(
            2,
            &level_name,
            "run-state setup",
            vec![format!("cannot create {}: {e}", tmp.display())],
        );
    }

    let mut plan = match build_plan(&root, &args, &tmp) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("validate: cannot build the execution plan: {e}");
            return RunSummary::refused(
                2,
                &level_name,
                "plan construction",
                vec![
                    e,
                    "no substitute profile was run: reporting a DIFFERENT gate set under the \
                     requested name would be worse than refusing"
                        .into(),
                ],
            );
        }
    };

    // Nested validate payloads are ordinary DAG children. Carry the selected
    // level through the plan so `--verbosity 5` does not become level 1 at the
    // nested strict-compat boundary (and default level 1 stays bounded there).
    propagate_verbosity(&mut plan, args.verbosity);

    // Per-gate budget overrides, preserved from validate.sh
    // (VALIDATE_GATE_TIMEOUT_SECONDS / VALIDATE_GATE_CPU_TIMEOUT_SECONDS). These
    // LOWER a node's ceiling, never raise it: a caller tightening budgets to
    // reproduce a timeout must not accidentally loosen a node that already
    // declared something stricter. They are also how the timeout path is
    // exercised on demand without waiting for a real runaway.
    if let Some(cap) = env_positive("VALIDATE_GATE_TIMEOUT_SECONDS") {
        clamp_wall(&mut plan, cap);
        eprintln!("validate: VALIDATE_GATE_TIMEOUT_SECONDS={cap}: every gate's wall ceiling lowered to at most {cap}s");
    }
    if let Some(cap) = env_positive("VALIDATE_GATE_CPU_TIMEOUT_SECONDS") {
        clamp_cpu(&mut plan, cap);
        eprintln!("validate: VALIDATE_GATE_CPU_TIMEOUT_SECONDS={cap}: every gate's CPU budget lowered to at most {cap}s");
    }

    // Fail-closed caps audit. A node without declared caps would run UNBOXED
    // while the driver still printed "boxing ACTIVE" — a green verifying less
    // than it claims. Refuse rather than run.
    // FAIL CLOSED on capacity that can never be granted. A step demanding a
    // resource the config does not cap is unschedulable forever, and the
    // scheduler expresses that as an infinite 50 ms sleep, not an error --
    // measured: 21 of ~58 nodes done, then 14 minutes at 0% CPU with no exit.
    // Refuse here so it is a named refusal before anything runs.
    let mut ungrantable = validate_plan::ungrantable_resources(&plan.cfg);
    if let Some(second) = &plan.second {
        ungrantable.extend(validate_plan::ungrantable_resources(second));
    }
    if !ungrantable.is_empty() {
        return RunSummary::refused(
            3,
            &plan.profile,
            "ungrantable scarce-resource demand",
            vec![
                format!("{} step(s) demand capacity the DAG config never grants:", ungrantable.len()),
            ]
            .into_iter()
            .chain(ungrantable.iter().take(8).map(|b| format!("  {b}")))
            .chain(std::iter::once(
                "the scheduler would sleep forever rather than fail: its only exit is                  running.is_empty() && done+skipped >= steps.len()".to_string(),
            ))
            .collect(),
        );
    }
    let mut undeclared = validate_plan::undeclared_nodes(&plan.cfg);
    if let Some(second) = &plan.second {
        undeclared.extend(validate_plan::undeclared_nodes(second));
    }
    if !undeclared.is_empty() {
        eprintln!(
            "validate: ERROR: {} node(s) lack declared resource caps and would run UNBOXED: {}",
            undeclared.len(),
            undeclared.join(", ")
        );
        eprintln!("  Declare timeout + cpu_timeout + a memory hint for each; see scripts/lib/validate_plan.rs.");
        return RunSummary::refused(
            3,
            &plan.profile,
            "the declared-caps audit",
            vec![
                format!(
                    "{} node(s) would run UNBOXED while the driver claimed boxing was active: {}",
                    undeclared.len(),
                    undeclared.join(", ")
                ),
                "declare timeout + cpu_timeout + a memory hint for each; see \
                 scripts/lib/validate_plan.rs"
                    .into(),
            ],
        );
    }

    // The whole-run budget is the first boundary able to stop cumulative cost
    // while preserving evidence. Per-node caps cannot bound a sequence of legal
    // nodes, and the hosted job kill discards the diagnostic tail.
    // Refuse an inverted ladder before even `--show-plan` succeeds. A node with
    // an allowance at least as large as the run budget can only be cut by the
    // less-specific outer clock, losing attribution to the node.
    if let Some(secs) = run_timeout {
        let mut bad = steps_violating_run_timeout(&plan.cfg, secs);
        if let Some(second) = &plan.second {
            bad.extend(steps_violating_run_timeout(second, secs));
        }
        if !bad.is_empty() {
            bad.sort();
            bad.dedup();
            return RunSummary::refused(
                3,
                &plan.profile,
                "whole-run budget is not larger than every node budget",
                std::iter::once(format!(
                    "{} node(s) declare a wall budget >= the {secs}s whole-run budget:",
                    bad.len()
                ))
                .chain(bad.iter().take(8).map(|(tag, t)| format!("  {tag} ({t}s)")))
                .chain(std::iter::once(
                    "lower the named node budgets so each can diagnose itself before the whole-run boundary"
                        .to_string(),
                ))
                .collect(),
            );
        }
    }

    // Print the plan and exit. This makes "what will actually run, and under what
    // caps" reviewable without spending a validate slot — and it is how the
    // declared-caps claim above can be checked by eye rather than trusted.
    if args.show_plan {
        let mut all: Vec<&DagConfig> = vec![&plan.cfg];
        if let Some(s) = &plan.second {
            all.push(s);
        }
        println!("profile: {}  selection: {}", plan.profile, plan.selection_mode);
        for (i, cfg) in all.iter().enumerate() {
            println!("\n--- DAG {} of {} ({}) : {} node(s)", i + 1, all.len(), cfg.description, cfg.steps.len());
            println!("{:<40} {:>7} {:>7} {:>8}  {}", "node", "wall_s", "cpu_s", "mem", "deps");
            for s in &cfg.steps {
                let cpu = if s.cpu_timeout > 0 { s.cpu_timeout } else { cfg.default_step_cpu_timeout };
                let mem = s.hint.hard_mem_max_bytes.or(s.hint.rss_baseline_bytes).unwrap_or(0);
                println!(
                    "{:<40} {:>7} {:>7} {:>7}M  {}",
                    s.tag(), s.timeout, cpu, mem / (1024 * 1024), s.deps.join(",")
                );
            }
        }
        let total: usize = all.iter().map(|c| c.steps.len()).sum();
        println!("\ntotal boxed nodes: {total}; all have declared wall+cpu+memory caps (audited above).");
        return RunSummary::new(
            Verdict::PlanOnly,
            0,
            &plan.profile,
            vec![
                format!("--show-plan: {total} boxed node(s) printed, all with declared wall+cpu+memory caps"),
                "nothing was executed and no ledger row was written".into(),
            ],
        );
    }

    // ---- tree-keyed result cache (validate.sh:620/655) -------------------
    //
    // Runs BEFORE boxing and before the durable log, so a hit leaves no partial
    // state behind and appends no derived record — the same placement the bash
    // used. The key is the TREE hash, not the commit: a rebase or amend that
    // leaves content byte-identical is the same thing to validate, and keying on
    // the commit would re-run it. `--ignore-cache` forces a real run; a focused
    // or selective profile is never cached because `selection_mode == "full"` is
    // part of the key.
    let ledger = ledger_path(&root);
    let ledger_rows = validate_history::read_rows(&ledger);
    let tree = git_tree();
    let host = short_hostname();
    let toolchain = sh("rustc", &["--version"]).unwrap_or_else(|| "unknown".into());
    let cache = cache_state(&root);
    let cache_key = validate_history::CacheKey {
        tree: &tree,
        profile: &plan.profile,
        host: &host,
        toolchain: &toolchain,
    };
    // A nested payload never consults the cache: the outer run already did, and a
    // payload that "hit" would report a green for a lane it never ran.
    if !nesting.nested
        && !args.ignore_cache
        && plan.cacheable
        && !wt_dirty
        && !tree_dirty()
        && plan.selection_mode == "full"
    {
        if let Some(hit) = validate_history::cache_lookup(&ledger_rows, "pass", &cache_key) {
            println!("# ============================================================");
            println!("# validate CACHE HIT for tree {tree}");
            println!("#   (commit {})", git_sha());
            println!(
                "#   passed {} (wall {}, CPU {}, {} {} executed)",
                hit.finished_at,
                human_duration(hit.real_seconds),
                human_duration(hit.cpu_seconds),
                hit.executed,
                hit.executed_unit
            );
            println!(
                "#   from a run of commit {} by {} -- use --ignore-cache to force a real run",
                hit.commit, hit.producer
            );
            println!("#   profile={} host={host} toolchain={toolchain}", plan.profile);
            println!("#   NO gates ran this invocation; reused a clean, commit-anchored passing");
            println!("#   record (nonzero executed count, satisfied gate coverage) from the");
            println!("#   run-ledger ({}).", ledger.display());
            println!("# ============================================================");
            let _ = std::fs::remove_dir_all(&tmp);
            let mut s = RunSummary::new(
                Verdict::CacheHit,
                0,
                &plan.profile,
                vec![
                    format!(
                        "reused the passing record from {} (commit {}, producer {}), keyed on tree {tree}",
                        hit.finished_at, hit.commit, hit.producer
                    ),
                    format!(
                        "that run recorded {} {} executed with satisfied gate coverage; \
                         --ignore-cache forces a real run",
                        hit.executed, hit.executed_unit
                    ),
                ],
            );
            s.ledger = Some(ledger.clone());
            return s;
        }
        // A prior FAIL does NOT skip: it may be flaky or environmental, and only
        // a PASS satisfies the landing predicate. Note it and run.
        if let Some(prev) = validate_history::cache_lookup(&ledger_rows, "fail", &cache_key) {
            eprintln!(
                "# validate: tree {tree} has a prior FAIL record ({}) on this host+toolchain; \
                 running anyway (a fail may be flaky/environmental). Only a PASS satisfies the \
                 landing predicate.",
                prev.finished_at
            );
        }
    }

    match run_timeout {
        Some(secs) => eprintln!(
            "validate: whole-run budget {secs}s across lanes and retries; in-flight nodes are cut and rows flushed on breach"
        ),
        None => eprintln!(
            "validate: WARNING: no whole-run budget (--run-timeout / HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS); per-node caps do not bound cumulative wall time"
        ),
    }

    let cgroups: BoxedCgroups =
        match resolve_cgroups(args.allow_cgroup_failure, run_timeout, deadline_ns) {
            Ok(c) => {
                // Claim the re-entrancy marker HERE, not before resolve_cgroups.
                // On the default path resolve_cgroups re-execs into a transient
                // systemd scope and does not return, so the process that reaches
                // this line is the one that will actually drive the run -- and it is
                // the only one whose pid a nested payload should see. Claiming
                // earlier made the driver read its own boxing re-exec as a nested
                // invocation and refuse itself.
                validate_runtime::claim_active_marker();
                c
            }
            Err(code) => {
                return RunSummary::refused(
                    code,
                    &plan.profile,
                    "cgroup boxing (fail-closed)",
                    vec![
                        "two-level cgroup-v2 boxing could not be established; see the message above"
                            .into(),
                        "resource boxing is this tool's primary purpose — re-run with \
                         --allow-cgroup-failure to accept an UNBOXED run"
                            .into(),
                    ],
                )
            }
        };

    let commit = git_sha();
    match setup_durable_log(&root, &plan.profile, &commit) {
        Ok(d) => *durable_slot = Some(d),
        Err(code) => {
            return RunSummary::refused(
                code,
                &plan.profile,
                "durable-log setup",
                vec![
                    "a run with no durable receipt is a silent no-result; see the message above"
                        .into(),
                ],
            )
        }
    }
    // Safe: just assigned. Cloned so the summary and the ledger can both name it
    // without borrowing the live tee handle.
    let log_path = durable_slot.as_ref().map(|d| d.path.clone()).unwrap_or_default();

    // ---- box-wide concurrency observation (validate.sh:1499) -----------------
    //
    // PORTED CORRECTED, NOT VERBATIM. The bash counted process-group EXISTENCE
    // (`ps -eo pgid=,args=` matching `validate\.sh`), so a parked stop-test
    // fixture counted identically to a 22-core validate. That is not a modelling
    // nicety: measured on this box 2026-08-07 the six live `validate.sh` process
    // groups were ALL orphaned fixtures at CPU/wall ~0.00, and the shipped ledger
    // carries `concurrent_validates` up to 20 as a result.
    //
    // Here a peer must clear two observable bars: it REGISTERED itself as a
    // top-level driver (so nested payloads and fixtures are excluded by
    // construction, not by filtering), its registration flock is still held (so
    // liveness is the kernel's answer, not a pid guess), and its process tree
    // BURNED CPU between two samples. A running peak is kept for the whole run
    // because a point-in-time probe misses a peer that starts and ends in the
    // middle.
    let registry = validate_runtime::registry_dir(parent.as_deref());
    let run_record = if nesting.nested {
        None
    } else {
        validate_runtime::register_run(&registry, &plan.profile, &root)
    };
    let monitor = if nesting.nested {
        None
    } else {
        Some(validate_runtime::ConcurrencyMonitor::start(
            registry.clone(),
            std::time::Duration::from_secs(2),
        ))
    };
    if nesting.nested {
        println!(
            "Nested validate (payload of outer pid {}): focused mode {} only; the outer run owns \
             the ledger, receipt, cache, invocation lock and concurrency accounting.",
            nesting.outer_pid.unwrap_or(-1),
            plan.profile
        );
    }

    let jobs = args.jobs.unwrap_or_else(default_jobs);
    let started_at = utc_now();
    let started_epoch = epoch_now();
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let node_count = plan.cfg.steps.len() + plan.second.as_ref().map(|c| c.steps.len()).unwrap_or(0);

    println!("Validation profile: {} (selection: {})", plan.profile, plan.selection_mode);
    println!("Commit: {commit} ({})", if tree_dirty() { "⚠️  NOT commit-anchored: dirty tree" } else { "clean tree, commit-anchored" });
    println!("Build cache: {cache}; host cores: {host_cpus}; scheduler width: -j {jobs}");
    println!("Plan: {node_count} boxed DAG node(s){}", if plan.second.is_some() { " across 2 sequential lanes" } else { "" });
    // A measured estimate from THIS machine's own history, or an honest "not
    // enough history" (validate.sh:936). Printed after the durable log is
    // established so the receipt carries the prediction next to the outcome.
    println!(
        "Estimated time: {}",
        validate_history::history_estimate(&ledger_rows, &plan.profile, cache, &host, ledger.exists())
    );
    if plan.super_mode {
        println!(
            "Super stress: {} repetitions/probe scheduled as individual boxed nodes at -j {jobs} \
             ({host_cpus} online CPUs)",
            validate_super::repetitions()
        );
    }

    // Level 1 is deliberately O(1) per step. The runner still captures every
    // byte and prints COMPLETE detail on failure; only passing chatter is
    // suppressed. Levels 2-4 stream tagged step output, while level 5 adds the
    // deepest observed test identity to every streamed line.
    let verbosity = args.verbosity;
    // The envelope profile is a MEASUREMENT: an eager exit on the first probe
    // failure would truncate the very vector it exists to produce.
    let keep_going = args.keep_going || plan.force_keep_going;

    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut ok = true;
    let mut env_retries = 0usize;

    // One clock for the whole invocation. Sequential lanes and retries spend
    // from the same allowance rather than each receiving a fresh 600 seconds.
    let deadline = deadline_ns;
    let lane = |cfg: &DagConfig| -> LaneResult {
        run_lane_with_env_retries(
            cfg,
            jobs,
            keep_going,
            verbosity,
            cgroups.clone(),
            &log_path,
            deadline,
        )
    };
    let mut run_timed_out = false;

    let r = lane(&plan.cfg);
    outcomes.extend(r.outcomes.iter().cloned());
    skipped.extend(r.skipped.iter().cloned());
    ok = ok && r.ok;
    env_retries += r.env_retries;
    run_timed_out = run_timed_out || r.run_timed_out;

    if let Some(second) = &plan.second {
        if ok || keep_going {
            let r2 = lane(second);
            outcomes.extend(r2.outcomes.iter().cloned());
            skipped.extend(r2.skipped.iter().cloned());
            ok = ok && r2.ok;
            env_retries += r2.env_retries;
            run_timed_out = run_timed_out || r2.run_timed_out;
        } else {
            eprintln!("validate: first lane failed; skipping the second lane (eager exit).");
        }
    }

    let wall = (epoch_now() - started_epoch) as f64;
    if run_timed_out {
        println!(
            "⏱ VALIDATE RUN BUDGET EXCEEDED after {wall:.0}s (budget {}s): remaining work was \
             cut so its node identities and rows could still be reported. This is an incomplete \
             judgement, not a product verdict.",
            run_timeout.unwrap_or(0)
        );
    }
    print_cost_table(&outcomes, &skipped);

    // ---- the single cleanup / evidence-commit point (validate.sh:1812) -------
    //
    // From here to the ledger append is ONE critical section. A second stop
    // signal must not abort it between teardown and the append, or a run that did
    // real work would leave no record of having run at all — which reads exactly
    // like never having started. `SIG_IGN` for the window is what `trap ''
    // INT TERM HUP` bought the bash.
    validate_runtime::enter_cleanup_critical_section();
    let interruption = interrupted_by().map(|s| s.to_string());
    // Stop the monitor and take the peak ONCE, here, so the ledger and the
    // summary cannot disagree about how crowded the box was.
    let (peak_active, peak_live) = match &monitor {
        Some(m) => {
            let (a, l) = m.finish();
            (Some(a as i64), Some(l as i64))
        }
        None => (None, None),
    };
    // Whole-run CPU, taken once in THIS process (a worker thread would see only
    // its own accounting, exactly as a bash subshell's `times` would).
    let (cpu_user, cpu_sys) = validate_runtime::process_cpu_seconds();
    let (executed_tests, filtered_tests) = libtest_counts(parent.as_deref(), &log_path);
    if executed_tests.is_none() {
        eprintln!(
            "validate: WARNING: libtest counts are UNKNOWN for this run. A ledger row with \
             executed_tests=null is a NON-VERDICT, not a green: no downstream completeness \
             predicate can qualify it."
        );
    }

    // Coverage and base identities come from the parent's single finalizer. A
    // local reconstruction here would create a second receipt authority.
    let receipt = receipt_evidence(parent.as_deref(), &root, &log_path, &commit);
    let coverage = receipt.coverage.clone();

    let behind_ahead = sh("git", &["rev-list", "--left-right", "--count", "origin/main...HEAD"])
        .unwrap_or_else(|| "0 0".into());
    let mut ba = behind_ahead.split_whitespace();
    let git_behind: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let git_ahead: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let dirty_now = tree_dirty();
    let commit_anchored = commit != "unknown" && !dirty_now;
    // Observed, not inferred: did the pin gate actually run and pass in THIS run?
    let pin_gate_passed = outcomes.iter().any(|o| o.tag == PIN_GATE_TAG && o.ok);
    let lock_admitted = canonical_validate_lock_admission(parent.as_deref(), &commit, &host);
    let ctx = LedgerCtx {
        started_at,
        host: host.clone(),
        toolchain: toolchain.clone(),
        slot: slot_name(&root, parent.as_deref()),
        cwd: root.to_string_lossy().into(),
        profile: plan.profile.clone(),
        selection_mode: plan.selection_mode.into(),
        cache_state: cache.into(),
        commit: commit.clone(),
        tree: git_tree(),
        git_ahead,
        git_behind,
        commit_anchored,
        tree_dirty: dirty_now,
        dag_jobs: jobs,
        admission: lock_admitted.then_some("ci-hub-validate-lock"),
        base_sha: receipt.base_sha,
        base_tree: receipt.base_tree,
        reverie_base_sha: receipt.reverie_base_sha,
        reverie_base_tree: receipt.reverie_base_tree,
        reverie_pin_current: pin_gate_passed,
        concurrent_validates: if lock_admitted { Some(0) } else { peak_active },
        concurrency_proof: if lock_admitted {
            Some("validate_lock_owner_ancestry")
        } else {
            peak_active.map(|_| "live_flock_registry_cpu_delta")
        },
        interruption: interruption.clone(),
        cpu_user,
        cpu_sys,
        env_block_retries: env_retries as i64,
        executed_tests,
        filtered_tests,
    };
    if let (Some(a), Some(l)) = (peak_active, peak_live) {
        println!(
            "Peer validates: {a} peak CPU-active of {l} peak live top-level run(s) registered in \
             {} (existence alone is not concurrency; each peer had to hold its own flock AND burn \
             CPU between two samples).",
            registry.display()
        );
    }

    // An operator stop is a NO-RESULT, and it is RECORDED as one. It is not
    // silently dropped: `scripts/test_validate_stop_paths.py` is the durable
    // consumer contract for exactly this row (result `no_result`, raw_result
    // `fail`, interruption_signal named), and every reader already knows the
    // no_result verdict. A TIMEOUT, by contrast, is a completed run and falls
    // through to the normal verdict below.
    if let Some(sig) = &interruption {
        if !nesting.nested {
            write_ledger(
                &ledger,
                &ctx,
                &outcomes,
                &skipped,
                wall,
                130,
                &log_path.to_string_lossy(),
                false,
                coverage.clone(),
            );
        }
        drop(run_record);
        let _ = std::fs::remove_dir_all(&tmp);
        let mut s = RunSummary::new(
            Verdict::Interrupted,
            130,
            &plan.profile,
            vec![
                format!("stopped by SIG{sig}; recorded as a NO-RESULT, not a failure"),
                "an interrupt learned nothing about the tree, so it does not establish a product \
                 verdict — a TIMEOUT, by contrast, does"
                    .into(),
            ],
        );
        s.nodes_executed = outcomes.len();
        s.nodes_failed = outcomes.iter().filter(|o| !o.ok && !o.aborted).count();
        s.nodes_skipped = skipped.len();
        s.wall_s = Some(wall);
        s.jobs = Some(jobs);
        s.log = Some(log_path);
        s.cpu_wall = Some((wall, cpu_user, cpu_sys));
        if !nesting.nested {
            s.ledger = Some(ledger);
        }
        return s;
    }

    // Compatibility ratchet, evaluated from typed outcomes.
    let mut compat_blocking = 0usize;
    // Carried to the verdict: a compat profile that measured nothing must not be
    // able to reach PASS through an empty set of failing rows.
    let mut compat_measured: Option<usize> = None;
    if let Some(mode) = plan.compat {
        let (passed, measured, blocking) = print_compat_summary(mode, &outcomes);
        compat_blocking = blocking.len();
        compat_measured = Some(measured);
        let floor = match mode {
            CompatMode::Sabre => Some(validate_corpus::SABRE_COMPAT_EXPECTED),
            CompatMode::Rr => Some(validate_corpus::RR_COMPAT_EXPECTED),
            _ => None,
        };
        if let Some(f) = floor {
            if passed < f {
                println!("❌ {} ratchet: {passed}/{measured} passing, floor {f} — BELOW FLOOR", mode.assurance());
                ok = false;
            } else {
                println!("✅ {} ratchet: {passed}/{measured} passing, floor {f} — met", mode.assurance());
            }
        }
        if !blocking.is_empty() {
            println!("❌ {} blocking failures ({}): {}", mode.assurance(), blocking.len(), blocking.join(", "));
        }
    }

    // Super stress pass rates, from typed outcomes rather than a scraped report.
    let mut super_blocking = 0usize;
    if plan.super_mode {
        let reps = validate_super::repetitions();
        let rates = validate_super::stress_rates(&outcomes, reps);
        super_blocking = validate_super::stress_verdict(&rates, reps, jobs, host_cpus);
    }

    // Working-envelope vector: score, emit JSON, print the human summary, and
    // enforce monotonicity when a baseline was supplied.
    let mut envelope_regressed = false;
    let mut envelope_error: Option<(u8, String)> = None;
    if let Some(env) = &plan.envelope {
        let short = sh("git", &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let vector = validate_envelope::score(&outcomes, env.reps, &short);
        let json_file = validate_envelope::json_path(&root);
        let text = validate_envelope::to_ordered_json(&vector);
        if let Err(e) = std::fs::write(&json_file, format!("{text}\n")) {
            eprintln!("validate: warning: cannot write {}: {e}", json_file.display());
        }
        validate_envelope::print_summary(&vector, env.reps, &json_file);
        if let Some(baseline) = &env.baseline {
            match validate_envelope::compare(&vector, baseline) {
                Ok(reg) => envelope_regressed = reg,
                Err((code, msg)) => {
                    eprintln!("{msg}");
                    envelope_error = Some((code, msg));
                }
            }
        }
    }

    let failures = outcomes.iter().filter(|o| !o.ok && !o.aborted).count();
    // The verdict is the RATCHET, not the raw node count.
    //
    // Three profiles deliberately have a verdict narrower than "every node
    // passed", and each states which rows it excluded and why:
    //   * compat — known fail-closed rows and bounded portable diagnostics are
    //     nonblocking by policy;
    //   * super — the KVM/DBI stress rows were unreachable in validate.sh, so
    //     their first measurement is reported rather than ratcheted;
    //   * envelope — it is a measurement, so probe failures lower a count and
    //     only the build/preflight spine can fail it.
    let blocking_failures = outcomes
        .iter()
        .filter(|o| !o.ok && !o.aborted && !plan.nonblocking.contains(&o.tag))
        .count();
    // Failures OUTSIDE the measured matrix: the build/prep/gate spine. `compat.*`
    // rows are excluded because the compat ratchet already judges them (and
    // excuses the known-fail-closed ones), so counting them here would both
    // double-count and re-block rows policy has excused. Everything else — a
    // failed `compatprep.*`, `pre.*`, `gate.*`, `build.*` — is a node whose
    // failure can EMPTY the matrix, and no matrix ratchet can speak to that.
    let structural_failures = outcomes
        .iter()
        .filter(|o| {
            !o.ok
                && !o.aborted
                && !o.tag.starts_with("compat.")
                && !plan.nonblocking.contains(&o.tag)
        })
        .count();
    let effective_failures = if plan.compat.is_some() {
        compat_blocking + structural_failures
    } else if plan.super_mode {
        blocking_failures + super_blocking
    } else {
        blocking_failures
    };
    let mut exit_code: u8 = if effective_failures == 0 { 0 } else { 1 };
    // `ok` from the runner reflects every node, including the nonblocking ones,
    // so it is only authoritative when nothing is excused.
    if plan.nonblocking.is_empty() && plan.compat.is_none() && !ok {
        exit_code = 1;
    }
    if envelope_regressed {
        exit_code = 1;
    }
    if let Some((code, _)) = &envelope_error {
        exit_code = *code;
    }

    // Completeness is not the ratchet's to decide. A ratchet narrows WHICH
    // measured rows may fail; it cannot answer whether anything was measured, so
    // these conditions are checked separately and named individually.
    let refusals = verdict_refusals(compat_measured, structural_failures, executed_tests);
    if exit_code == 0 && !refusals.is_empty() {
        for why in &refusals {
            eprintln!("validate: ERROR: {why}");
        }
        eprintln!(
            "validate: refusing to report PASS: the run did not measure enough to certify \
             anything."
        );
        exit_code = 1;
    }

    // Receipt production is itself an enforcement path (validate.sh:1846).
    //
    // Every profile plans `pre.reverie_pin` and every lane node depends on it, so
    // in principle a green cannot happen without it. This asserts that anyway: if
    // a future fast path, cache branch, or early return ever bypasses the pin
    // gate, it must not emit PASS merely because the tests it did select happened
    // to pass. The archival pin is not a testing exemption, and "the DAG makes it
    // impossible" is a structural argument, not an observation of this run.
    let mut pin_gate_bypassed = false;
    if exit_code == 0 && !pin_gate_passed {
        eprintln!(
            "validate: ERROR: this path produced a PASS without a passing {PIN_GATE_TAG} gate; \
             refusing a passing receipt."
        );
        exit_code = 1;
        pin_gate_bypassed = true;
    }

    // A NESTED payload writes nothing: the outer run owns the ledger and the
    // receipt, and a second row for one logical run is exactly the duplication
    // the re-entrancy guard exists to prevent.
    if !nesting.nested {
        write_ledger(
            &ledger,
            &ctx,
            &outcomes,
            &skipped,
            wall,
            exit_code,
            &log_path.to_string_lossy(),
            plan.suite_complete,
            coverage,
        );
    }

    // Receipt publication, strictly AFTER the ledger append: `ci-hub
    // apply-local-label` re-derives the receipt FROM the ledger, so publishing
    // first would label the PR from the previous run's newest row. Non-fatal by
    // contract — the exit code is already decided above and nothing here can
    // change it (validate.sh:1735).
    match validate_receipt::eligible(
        exit_code,
        effective_failures,
        args.label_pr && !nesting.nested,
        commit_anchored,
        dirty_now,
        &plan.profile,
    ) {
        Ok(()) => {
            let _ = validate_receipt::publish();
        }
        Err(why) => {
            if args.verbosity >= 2 {
                eprintln!("validate: not publishing a receipt-backed label: {why}");
            }
        }
    }

    drop(run_record);
    let _ = std::fs::remove_dir_all(&tmp);

    // The completed-run summary. Names the excused rows explicitly, so a green
    // verdict that ignored some failures can never read as "everything passed".
    let mut detail = Vec::new();
    let excused = failures - blocking_failures;
    if exit_code == 0 {
        detail.push(format!("every blocking gate passed ({} node(s) ran)", outcomes.len()));
    } else {
        let named: Vec<&str> = outcomes
            .iter()
            .filter(|o| !o.ok && !o.aborted && !plan.nonblocking.contains(&o.tag))
            .map(|o| o.tag.as_str())
            .take(8)
            .collect();
        detail.push(format!(
            "{effective_failures} blocking failure(s){}",
            if named.is_empty() { String::new() } else { format!(": {}", named.join(", ")) }
        ));
    }
    if excused > 0 {
        detail.push(format!(
            "{excused} failing node(s) were NONBLOCKING by policy and excluded from the verdict \
             (see the ratchet lines above for which and why)"
        ));
    }
    // A nonzero exit that came from the envelope comparison rather than from a
    // gate must SAY so: "0 blocking failure(s)" beside exit 2 is unreadable.
    if envelope_regressed {
        detail.push(
            "the working-envelope vector REGRESSED below its baseline (see the monotonicity \
             table above); no gate failed"
                .into(),
        );
    }
    if let Some((_, msg)) = &envelope_error {
        detail.push(format!("envelope comparison could not run: {msg}"));
    }
    if !timed_out_nodes(&outcomes).is_empty() {
        detail.push(format!(
            "{} node(s) hit a wall or CPU budget; a timeout IS a recorded result: {}",
            timed_out_nodes(&outcomes).len(),
            timed_out_nodes(&outcomes).join(", ")
        ));
    }
    if !skipped.is_empty() {
        detail.push(format!("{} node(s) never ran because a dependency failed", skipped.len()));
    }
    for why in &refusals {
        detail.push(format!("REFUSED ON COMPLETENESS: {why}"));
    }
    if pin_gate_bypassed {
        detail.push(
            "this path reached a PASS without a passing pre.reverie_pin gate; the receipt was \
             REFUSED and the verdict forced to fail (the archival pin is not a testing exemption)"
                .into(),
        );
    }
    if env_retries > 0 {
        detail.push(format!(
            "{env_retries} environmental retry round(s) were spent on host/sandbox blocks; this \
             verdict did NOT pass on the first attempt"
        ));
    }
    match executed_tests {
        Some(n) => detail.push(format!(
            "{n} test(s) executed, {} filtered (parsed from the durable log by the parent's \
             single-sourced banner parser)",
            filtered_tests.map(|f| f.to_string()).unwrap_or_else(|| "unknown".into())
        )),
        None => detail.push(
            "executed_tests is UNKNOWN — this row is a NON-VERDICT and cannot qualify a receipt, \
             whatever the exit code says"
                .into(),
        ),
    }
    let mut s = RunSummary::new(
        if exit_code == 0 { Verdict::Pass } else { Verdict::Fail },
        exit_code,
        &plan.profile,
        detail,
    );
    s.nodes_executed = outcomes.len();
    s.nodes_failed = failures;
    s.nodes_skipped = skipped.len();
    s.wall_s = Some(wall);
    s.jobs = Some(jobs);
    s.log = Some(log_path);
    s.cpu_wall = Some((wall, cpu_user, cpu_sys));
    if !nesting.nested {
        s.ledger = Some(ledger);
    }
    s
}

// ------------------------------------------------------------- stop-path seam

/// The `HERMIT_VALIDATE_STOP_TEST_MODE` fixture (validate.sh:1899).
///
/// It exercises this driver's REAL stop handlers and REAL ledger writer without
/// starting a product build, which is the only way to test the signal paths in
/// bounded time. It cannot produce a pass: it records two synthetic gates and
/// then waits to be stopped. `scripts/test_validate_stop_paths.py` is its
/// consumer and asserts the exact row shape produced here.
///
/// # The leak this closes
///
/// The fixture parks until its parent test signals it, and the test spawns it
/// with `start_new_session=True` — so if the test dies first (an assertion before
/// the signal, a `wait` timeout, or the agent being recycled) nothing ever
/// signals it, and nothing in its new session can. Measured on this box
/// 2026-08-07: six orphaned `validate.sh full` process groups, all `ppid=1`, ages
/// 2h20m to 4h30m, each parked in `sleep 1` at CPU/wall ~0.00. Two exits now make
/// that unrepresentable — orphan detection (`getppid() == 1`) and a lifetime
/// deadline — and the Python harness additionally tears its own child's process
/// group down in a `finally`.
fn stop_test_seam(root: &Path, profile: &str, parent: Option<&Path>) -> RunSummary {
    let started_at = utc_now();
    let started = std::time::Instant::now();
    let prior_failure = env_flag("VALIDATE_STOP_TEST_PRIOR_FAILURE", "1");
    let synth = |name: &str, ok: bool| StepOutcome {
        tag: name.to_string(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        returncode: Some(if ok { 0 } else { 1 }),
        reason: if ok { String::new() } else { "stop-test synthetic failure".into() },
        aborted: false,
    };
    let outcomes =
        vec![synth("stop-test completed gate 1", !prior_failure), synth("stop-test completed gate 2", true)];

    validate_runtime::stop_test_announce();
    let exit = validate_runtime::stop_test_park(interrupted_by);

    // Cleanup is the evidence-commit point: make it signal-atomic BEFORE the
    // readiness hook fires, because the cleanup-race case then hammers this
    // process with SIGTERM and must not be able to abort the single append.
    validate_runtime::enter_cleanup_critical_section();
    validate_runtime::stop_test_cleanup_hook();

    let interruption = match exit {
        validate_runtime::StopTestExit::Signalled => interrupted_by().map(|s| s.to_string()),
        _ => None,
    };
    let exit_code: u8 = if interruption.is_some() { 130 } else { 1 };
    let (cpu_user, cpu_sys) = validate_runtime::process_cpu_seconds();
    let wall = started.elapsed().as_secs_f64();
    let ledger = ledger_path(root);
    let host = short_hostname();
    let commit = git_sha();
    let lock_admitted = canonical_validate_lock_admission(parent, &commit, &host);
    let ctx = LedgerCtx {
        started_at,
        host,
        toolchain: sh("rustc", &["--version"]).unwrap_or_else(|| "unknown".into()),
        slot: slot_name(root, parent),
        cwd: root.to_string_lossy().into(),
        profile: profile.to_string(),
        selection_mode: "full".into(),
        cache_state: cache_state(root).into(),
        commit,
        tree: git_tree(),
        git_ahead: 0,
        git_behind: 0,
        commit_anchored: false,
        tree_dirty: tree_dirty(),
        dag_jobs: 0,
        admission: lock_admitted.then_some("ci-hub-validate-lock"),
        base_sha: serde_json::Value::Null,
        base_tree: serde_json::Value::Null,
        reverie_base_sha: serde_json::Value::Null,
        reverie_base_tree: serde_json::Value::Null,
        // The fixture runs no gates at all, so it never observed the pin gate.
        reverie_pin_current: false,
        // The fixture never registers as a top-level driver, so it can neither
        // observe peers nor be counted as one.
        concurrent_validates: lock_admitted.then_some(0),
        concurrency_proof: lock_admitted.then_some("validate_lock_owner_ancestry"),
        interruption: interruption.clone(),
        cpu_user,
        cpu_sys,
        env_block_retries: 0,
        executed_tests: None,
        filtered_tests: None,
    };
    // `suite_complete: false` — a fixture that ran two synthetic gates must never
    // publish a gates_expected obligation, which is what would make it look like
    // a completed full profile.
    write_ledger(&ledger, &ctx, &outcomes, &[], wall, exit_code, "", false, serde_json::json!({}));

    let detail = match exit {
        validate_runtime::StopTestExit::Signalled => vec![format!(
            "stop-path fixture: stopped by SIG{}; recorded as {}",
            interruption.clone().unwrap_or_default(),
            if prior_failure { "fail (a completed gate had already failed)" } else { "no_result" }
        )],
        validate_runtime::StopTestExit::EarlyExit => vec![
            "stop-path fixture: VALIDATE_STOP_TEST_EXIT_EARLY — an ordinary incomplete exit, NOT \
             an operator stop, so the row stays a raw fail with no interruption signal"
                .into(),
        ],
        validate_runtime::StopTestExit::Orphaned => vec![
            "stop-path fixture: ORPHANED (getppid()==1) — the test that spawned it died without \
             signalling, so it self-terminated instead of parking forever"
                .into(),
        ],
        validate_runtime::StopTestExit::Deadline => vec![
            "stop-path fixture: lifetime deadline expired (VALIDATE_STOP_TEST_MAX_SECONDS); \
             self-terminated rather than leaking a parked process group"
                .into(),
        ],
    };
    let mut s = RunSummary::new(
        if interruption.is_some() { Verdict::Interrupted } else { Verdict::Fail },
        exit_code,
        profile,
        detail,
    );
    s.nodes_executed = outcomes.len();
    s.nodes_failed = outcomes.iter().filter(|o| !o.ok).count();
    s.wall_s = Some(wall);
    s.cpu_wall = Some((wall, cpu_user, cpu_sys));
    s.ledger = Some(ledger);
    s
}
