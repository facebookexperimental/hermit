#!/usr/bin/env -S rust-script --force
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
//! * **Everything runs as a `dagrun` node.** Preflight, the manifest
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
//! Most of the flag surface preserves the former driver's CLI because in-tree
//! callers depend on it — notably `.github/workflows/validation-levels.yml`,
//! three `Makefile` targets, and `hermit-cli/tests/{analyze,rr_suite}.rs`. The
//! inner dirty-tree and rebase-freshness escape names its limited scope
//! explicitly; its in-tree callers are updated with it.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! hermit-manifest-plan = { path = "../ci/manifest-plan" }
//! serde_json = "1"
//! toml = "0.8.23"
//! sha2 = "0.10"
//! libc = "0.2"
//! tempfile = "3"
//! shell-words = "1.1"
//! ```

// `serde_json::json!` expands one recursive macro level PER FIELD, and the ledger
// record is one literal carrying every qualification a reader needs. Keeping it a
// single literal is the point — it is what makes "the row states its own
// conditions" checkable by eye — so the limit is raised rather than the record
// split across statements where a field could be added on one path and not the
// other.
#![recursion_limit = "512"]

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "lib/validate_corpus.rs"]
mod validate_corpus;

#[path = "lib/validate_envelope.rs"]
mod validate_envelope;

#[path = "lib/validate_history.rs"]
mod validate_history;

#[path = "lib/validate_cell_results.rs"]
mod validate_cell_results;

#[path = "lib/validate_plan.rs"]
mod validate_plan;

#[path = "lib/validate_receipt.rs"]
mod validate_receipt;

#[path = "lib/validate_runtime.rs"]
mod validate_runtime;

#[path = "lib/validate_classification.rs"]
mod validate_classification;

use validate_classification::{
    NodeClassification, attempt_classification, classify_run, node_classification,
    validation_completeness_detail, validation_is_complete,
};

#[path = "lib/safe_ci_scope.rs"]
mod safe_ci_scope;

#[path = "lib/validate_super.rs"]
mod validate_super; // Normalizes and audits extracted Cargo tests/synthetic args onto nextest.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use sha2::Digest;
use sha2::Sha256;
use dagrun::cgroup::aggregate_slice_max_cpus;
use dagrun::cgroup::is_in_scope;
use dagrun::cgroup::observe_own_containment;
use dagrun::io::dag_from_json;
use dagrun::io::dag_to_json;
use dagrun::model::CmdType;
use dagrun::model::DagConfig;
use dagrun::model::DagManifest;
use dagrun::model::ResultManifest;
use dagrun::model::RunResult;
use dagrun::model::Step;
use dagrun::model::StepOutcome;
use dagrun::model::StructuredTestResultsManifest;
use dagrun::TestResult;
use dagrun::TestResults;
use dagrun::container_core_budget;
use dagrun::perflog::append_step_profiles;
use dagrun::scheduler::run_dag_boxed_deadline;
use dagrun::scheduler::steps_violating_run_timeout;
use dagrun::scheduler::BoxedCgroups;
use dagrun::scheduler::monotonic_now_ns;
use dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV;
use hermit_manifest_plan::ledger::HistoryRow;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::resolved_cell_timeouts;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::E2E_KERNEL_VERSION_ENV;
use hermit_manifest_plan::runner::E2E_MACHINE_SHORTNAME_ENV;
use hermit_manifest_plan::service_result::FinalValidateStatus;
use hermit_manifest_plan::service_result::ScorecardWriteback;
use hermit_manifest_plan::service_result::ValidationServiceResult;
use hermit_manifest_plan::timeouts::DEFAULT_TEST_WALL_TIMEOUT_SECONDS;
use hermit_manifest_plan::timeouts::TEST_CPU_TIMEOUT_MULTIPLIER_ENV;
use hermit_manifest_plan::timeouts::scale_timeout_seconds;
use hermit_manifest_plan::timeouts::timeout_multipliers_from_env;
use hermit_manifest_plan::timeouts::TimeoutMultipliers;
use hermit_manifest_plan::timeouts::parse_timeout_multiplier;
use hermit_manifest_plan::timeouts::TEST_WALL_TIMEOUT_MULTIPLIER_ENV;
#[cfg(test)]
use hermit_manifest_plan::timeouts::{
    DEFAULT_TEST_CPU_TIMEOUT_SECONDS, resolve_test_timeouts,
};

use validate_plan::CompatMode;
use validate_plan::CompatDisposition;

/// Current receipt schema. Unknown scalar evidence is represented by explicit
/// nulls; optional collections stay type-safe by being omitted when inapplicable
/// or serialized as `[]` when positively known empty. A new writer must never
/// downgrade itself into the schema-4 grandfather.
const COVERAGE_LEDGER_SCHEMA_VERSION: i64 = 5;

/// Recorded in each row so a version-aware reader can tell which driver produced
/// it without inference.
const LEDGER_PRODUCER: &str = "hermit-validate-rs";

/// The Reverie-pin preflight node's tag. Named once so the plan that creates it
/// and the fail-closed assertion that requires it cannot drift apart.
const PIN_GATE_TAG: &str = "pre.reverie_pin";
const MANIFEST_AUDIT_COMMAND: &str = validate_plan::MANIFEST_AUDIT_COMMAND;
const INTEGRATION_ARTIFACT_WRAPPER: &str =
    "./ci/run-with-hermit-e2e-artifact.sh --require-install ";
/// Compatibility-selection alias retained for CLI callers that used the old
/// placeholder tag. The committed DAG contains the real `compat.*` population;
/// this name is never a node and never triggers runtime graph generation.
const STRICT_COMPAT_SELECTION_ALIAS: &str = "test.strict_compat";
const NEXTEST_PORTABLE_PREPARE_COMMAND: &str = "./ci/nextest-binaries.rs prepare portable";
const NEXTEST_PRIVILEGED_ASSERT_COMMAND: &str = "./ci/nextest-binaries.rs assert privileged";
const TESTS_MISC_EXECUTABLE_READ_COMMAND: &str = r#"tests_misc="$(./ci/nextest-binaries.rs executable hermit-detcore tests_misc)" || exit 1"#;


fn hermit_integration_uses_published_artifact(step: &Step) -> bool {
    step
        .cmd
        .strip_prefix(RUST_SCRIPT_COMMAND_PREFIX)
        .is_some_and(|command| command.starts_with(INTEGRATION_ARTIFACT_WRAPPER))
        && step
            .deps
            .iter()
            .any(|dependency| dependency == "build.e2e_artifact")
}


#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationStepIdentity {
    ManifestAudit,
    ManifestRun,
    Other,
}

fn validation_step_identity(step: &Step) -> ValidationStepIdentity {
    let manifest_group = step.group == "e2e" || step.group.ends_with("-e2e");
    let quick_manifest_run = step.group == "quick" && step.job == "e2e_verify";
    if (step.group == "gate" && step.job == "manifest")
        || (manifest_group && step.job == "metadata")
    {
        ValidationStepIdentity::ManifestAudit
    } else if step.manifest.is_some() || quick_manifest_run {
        ValidationStepIdentity::ManifestRun
    } else {
        ValidationStepIdentity::Other
    }
}

const LEDGER_ENV: &str = "HERMIT_VALIDATE_LEDGER";
const PARENT_ENV: &str = "DEV_HERMIT_PARENT";
const TOOL_ROOT_ENV: &str = "DEV_HERMIT_TOOL_ROOT";
const TOOL_AUTHORITY_ENV: &str = "DEV_HERMIT_TOOL_AUTHORITY";
const TOOL_CONTENT_SHA256_ENV: &str = "DEV_HERMIT_TOOL_CONTENT_SHA256";
const TOOL_PARENT_SHA_ENV: &str = "DEV_HERMIT_TOOL_PARENT_SHA";
const TOOL_HERMIT_SHA_ENV: &str = "DEV_HERMIT_TOOL_HERMIT_SHA";
const TOOL_AGENT_UTILS_SHA_ENV: &str = "DEV_HERMIT_TOOL_AGENT_UTILS_SHA";
const TOOL_BOOTSTRAP_SHA256_ENV: &str = "DEV_HERMIT_TOOL_BOOTSTRAP_SHA256";
const TOOL_AUTHORITY_SCHEMA: &str = "dev-hermit-tool-authority-v1";
const OWN_SCOPE_DEADLINE_ENV: &str = "HERMIT_VALIDATE_SCOPE_DEADLINE_MONOTONIC_NS";
const NESTED_SCOPE_SELF_TEST_ENV: &str = "HERMIT_VALIDATE_NESTED_SCOPE_SELF_TEST";
const SUMMARY_EPILOGUE_SELF_TEST_ENV: &str = "HERMIT_VALIDATE_SUMMARY_EPILOGUE_SELF_TEST";
const NESTED_SCOPE_OUTER: &str = "outer";
const NESTED_SCOPE_INNER: &str = "inner";
const NESTED_SCOPE_SIGNAL: &str = "signal";
const NESTED_INNER_STEP_S: i64 = 2;
const NESTED_INNER_RUN_S: i64 = 5;
const NESTED_OUTER_CHILD_STEP_S: i64 = 10;
const NESTED_OUTER_CHILD_RUN_S: i64 = 12;
const NESTED_SIGNAL_STEP_S: i64 = 5;
const NESTED_SIGNAL_RUN_S: i64 = 7;
const NESTED_SURVIVOR_STEP_S: i64 = 2;
const NESTED_SURVIVOR_RUN_S: i64 = 4;
const NESTED_SCOPE_RUNTIME_S: i64 = 30;
const NESTED_WRAPPER_TIMEOUT_S: i64 = 45;

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
    HostedPortable,
    HostedPrivileged,
    RequalifyCell { test: String, mode: String, backend: String },
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
            Focused::HostedPortable => "hosted-portable".into(),
            Focused::HostedPrivileged => "hosted-privileged".into(),
            Focused::RequalifyCell { .. } => "cell-requalification".into(),
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
            Focused::HostedPortable => "hosted-portable-only",
            Focused::HostedPrivileged => "hosted-privileged-only",
            Focused::RequalifyCell { .. } => "requalify-cell",
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
    allow_local_off_the_record_run: bool,
    skip_inner_dirty_working_tree_and_rebase_freshness_checks: bool,
    ignore_cache: bool,
    label_pr: bool,
    no_label_pr_explicit: bool,
    verbosity: i64,
    jobs: Option<i64>,
    keep_going: bool,
    allow_cgroup_failure: bool,
    /// Wall budget for the whole validate invocation, across lanes and retries.
    run_timeout: Option<i64>,
    self_test: bool,
    show_plan: bool,
    show_plan_json: bool,
    write_constructed_dag: Option<PathBuf>,
    /// Generator-only output containing the corpus-derived DAG partition.
    write_generated_plan: Option<PathBuf>,
    selected: Option<String>,
    ignore_selected_deps: bool,
}

const SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION: &str =
    "--skip-inner-dirty-working-tree-and-rebase-freshness-checks";
const SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_ENV: &str =
    "VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS";
const ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION: &str = "--allow-local-off-the-record-run";

fn usage() -> &'static str {
    "Usage: ./scripts/validate.rs [LEVEL] [OPTIONS]\n\
     \n\
     Run Hermit's local validation suite. Every gate executes as a boxed\n\
     dagrun DAG node; nothing runs outside the runner.\n\
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
     \x20 --strict-compat-only          Run the blocking legacy stripped app matrix.\n\
     \x20 --portable-strict-compat-only Portable legacy stripped matrix with bounded diagnostics.\n\
     \x20 --rr-compat-only              Gate the known-passing record/replay matrix.\n\
     \x20 --sabre-compat-only           Gate the measured SaBRe matrix.\n\
     \x20 --e9patch-compat-only         Gate core + installed e9patch legacy stripped apps.\n\
     \x20 --liteinst-compat-only        Run the portable CI liteinst_strict test.\n\
     \x20 --qemu-l2-only                Run the heavyweight QEMU L2 boot.\n\
     \x20 --portable-only               No PMU/CPUID hardware required.\n\
     \x20 --privileged-only             PMU/CPUID-dependent tests only.\n\
     \x20 --hosted-portable-only        Select the committed host-execution graph used by\n\
     \x20                                the GitHub portable shard runner.\n\
     \x20 --hosted-privileged-only      Select the committed 12-node privileged host graph.\n\
     \x20 --requalify-cell TEST MODE BACKEND  Run the committed DAG step owning that exact cell.\n\
     \x20 --only <lane> <group.job>[,...]  Run those lane node(s) with their own\n\
     \x20                  declared caps; outside deps are dropped; preflight tags\n\
     \x20                  reuse validate's canonical preflight nodes.\n\
     \x20 --selective, --since-green    Only nodes affected since the last green baseline.\n\
     \x20 --shallow-select              Like --selective but pin the baseline to HEAD~1.\n\
     \x20 --baseline <sha>              Known-green baseline commit for --selective.\n\
     \x20 --envelope-only               Measure and emit the working-envelope vector (JSON + human).\n\
     \x20 --envelope-compare FILE       Measure, then fail if any count regressed below FILE.\n\
     \x20 --all, --full-run             Assert the COMPLETE suite explicitly.\n\
     \n\
     Other options:\n\
     \x20 --allow-local-off-the-record-run\n\
     \x20                  Permit a clean, commit-anchored quick or focused local run for\n\
     \x20                  iterative testing. It writes no ledger row, publishes no receipt,\n\
     \x20                  and cannot be cited as validation evidence.\n\
     \x20 --verbose        Verbosity level 2: stream tagged per-step output.\n\
     \x20 --verbosity N    Output level 1..5 (default 1; levels 3/4 currently equal 2;\n\
     \x20                  level 5 prefixes every streamed line with test identity).\n\
     \x20 --skip-inner-dirty-working-tree-and-rebase-freshness-checks\n\
     \x20                  Skip only scripts/validate.rs's dirty-working-tree and\n\
     \x20                  rebase-freshness checks; does not bypass ci-hub validate-lock\n\
     \x20                  admission. AGENTS SHOULD NOT USE THIS.\n\
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
     \x20 --show-plan      Print the outer boxed DAG nodes, caps, and dependencies and exit.\n\
     \x20                  It does not enumerate Rust test IDs or E2E cells.\n\
     \x20 --show-plan-json Print the selected committed plan as JSON.\n\
     \x20 --write-constructed-dag FILE\n\
     \x20                  Write the selected committed DagConfig JSON for ci/run-dag.sh.\n\
     \x20 --write-generated-plan FILE\n\
     \x20                  Generator-only: write corpus-derived compat/stress nodes.\n\
     \x20 --selected <group.job>[,...]  Select these IDs from the committed profile.\n\
     \x20 --ignore-selected-deps       Omit predecessors supplied by an external harness.\n\
     \x20 --self-test      Run inert policy/data brackets plus one bounded disposable\n\
     \x20                  nested-cgroup check, then exit.\n\
     \x20 -h, --help       Show this help and exit.\n\
     \n\
     Actual validation attempts end with exactly one machine-readable final line:\n\
     \x20 FINAL_VALIDATE_STATUS: PASSED          exit 0\n\
     \x20 FINAL_VALIDATE_STATUS: FAILED          exit 1\n\
     \x20 FINAL_VALIDATE_STATUS: COULD_NOT_RUN   exit 75\n\
     The line is validate's last output and reports the validation verdict. A\n\
     post-verdict scorecard write-back failure preserves that line and exits 75;\n\
     current readers distinguish the two through ValidationServiceResult. No line\n\
     means validate died before reporting.\n\
     Help, --show-plan, --write-constructed-dag, --write-generated-plan, and\n\
     --probe-host-capability do\n\
     not attempt validation and therefore do not emit a final validate status.\n\
     \n\
     Environment: VALIDATE_LEVEL, VALIDATE_LABEL_PR,\n\
     VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS,\n\
     VALIDATE_IGNORE_CACHE, VALIDATE_VERBOSITY, VALIDATE_VERBOSE, VALIDATE_FORCE_FULL,\n\
     HERMIT_VALIDATE_LEDGER, PR_NUMBER, SUPER_REPETITIONS, L4_REPS, ENVELOPE_JSON,\n\
     HERMIT_LAST_GREEN_SHA, CI_HUB_APPLY_LOCAL_LABEL, DEV_HERMIT_PARENT,\n\
     DEV_HERMIT_TOOL_ROOT.\n\
     \n\
     HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT=<name>[,<name>] asserts that this\n\
     machine HAS a declared host capability, so its nodes run without probing.\n\
     It is deliberately one-directional: it can only cause MORE nodes to run.\n\
     Nothing can force a capability ABSENT, because that would be a way to make\n\
     a node stop running without anyone measuring the machine.\n\
     \n\
     --probe-host-capability <name> reports this machine's verdict for one\n\
     capability as PRESENT|ABSENT plus the observation behind it, and exits.\n\
     It runs no gate. target/debug/test-harness calls it so a withheld manifest CELL\n\
     and a withheld DAG node are decided by the same probe."
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
        allow_local_off_the_record_run: false,
        skip_inner_dirty_working_tree_and_rebase_freshness_checks: env_flag(
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_ENV,
            "1",
        ),
        ignore_cache: env_flag("VALIDATE_IGNORE_CACHE", "1"),
        label_pr: !env_flag("VALIDATE_LABEL_PR", "0"),
        no_label_pr_explicit: false,
        verbosity,
        jobs: None,
        keep_going: false,
        allow_cgroup_failure: false,
        run_timeout: None,
        self_test: false,
        show_plan: false,
        show_plan_json: false,
        write_constructed_dag: None,
        write_generated_plan: None,
        selected: None,
        ignore_selected_deps: false,
    };
    let mut shallow = false;
    let mut selective = false;
    let mut show_plan = false;
    let mut show_plan_json = false;
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
            "--hosted-portable-only" => focused.push(Focused::HostedPortable),
            "--hosted-privileged-only" => focused.push(Focused::HostedPrivileged),
            "--requalify-cell" => {
                let test = argv.get(i + 1).cloned().unwrap_or_default();
                let mode = argv.get(i + 2).cloned().unwrap_or_default();
                let backend = argv.get(i + 3).cloned().unwrap_or_default();
                if test.is_empty() || mode.is_empty() || backend.is_empty() {
                    eprintln!("validate: --requalify-cell needs TEST MODE BACKEND");
                    return Err(2);
                }
                focused.push(Focused::RequalifyCell { test, mode, backend });
                i += 3;
            }
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
            "--show-plan-json" => {
                show_plan = true;
                show_plan_json = true;
            }
            "--write-constructed-dag" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => {
                        show_plan = true;
                        args.write_constructed_dag = Some(PathBuf::from(v));
                    }
                    _ => {
                        eprintln!("validate: --write-constructed-dag needs a FILE");
                        return Err(2);
                    }
                }
            }
            "--write-generated-plan" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => {
                        show_plan = true;
                        args.write_generated_plan = Some(PathBuf::from(v));
                    }
                    _ => {
                        eprintln!("validate: --write-generated-plan needs a FILE");
                        return Err(2);
                    }
                }
            }
            "--selected" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => args.selected = Some(v.clone()),
                    _ => {
                        eprintln!("validate: --selected needs <group.job>[,<group.job>...]");
                        return Err(2);
                    }
                }
            }
            "--ignore-selected-deps" => args.ignore_selected_deps = true,
            "--selective" | "--since-green" => selective = true,
            "--shallow-select" => {
                selective = true;
                shallow = true;
            }
            "--all" | "--full-run" => args.force_full = true,
            ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION => {
                args.allow_local_off_the_record_run = true;
                args.label_pr = false;
            }
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION => {
                args.skip_inner_dirty_working_tree_and_rebase_freshness_checks = true
            }
            "--ignore-cache" => args.ignore_cache = true,
            "--label-pr" => {
                args.label_pr = true;
                args.no_label_pr_explicit = false;
            }
            "--no-label-pr" => {
                args.label_pr = false;
                args.no_label_pr_explicit = true;
            }
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
            "--sequential-lanes" => {
                eprintln!(
                    "validate: --sequential-lanes was removed when validation moved to one labelled DAG; \
                     select the committed full graph instead"
                );
                return Err(2);
            }
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
    args.show_plan_json = show_plan_json;
    args.focused = focused.pop();
    let output_forms = usize::from(args.show_plan_json)
        + usize::from(args.write_constructed_dag.is_some())
        + usize::from(args.write_generated_plan.is_some());
    if output_forms > 1 {
        eprintln!("validate: choose only one constructed-plan output form");
        return Err(2);
    }
    if args.allow_local_off_the_record_run {
        args.label_pr = false;
    }
    if matches!(
        args.focused,
        Some(Focused::HostedPortable | Focused::HostedPrivileged)
    )
        && !args.allow_local_off_the_record_run
        && !args.show_plan
    {
        eprintln!("validate: hosted selections are off-record shard execution modes");
        return Err(2);
    }
    if args.ignore_selected_deps && args.selected.is_none() {
        eprintln!("validate: --ignore-selected-deps requires --selected");
        return Err(2);
    }
    if args.selected.is_some() && !args.allow_local_off_the_record_run && !args.show_plan {
        eprintln!(
            "validate: --selected is partial execution and requires \
             --allow-local-off-the-record-run; it cannot publish validation evidence"
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

/// The environment marker only routes an invocation that already parsed the
/// explicit `--self-test` flag. An inherited or operator-supplied marker must
/// never turn an ordinary validation into a small passing probe.
fn nested_scope_probe_selected(self_test: bool, marker_present: bool) -> bool {
    self_test && marker_present
}

fn nested_scope_probe_requested() -> bool {
    std::env::var_os(NESTED_SCOPE_SELF_TEST_ENV).is_some()
}

/// Every local rung is strict, and the three sequential outer runs sum to
/// 12 + 7 + 4 = 23s, below the disposable scope's 30s and wrapper's 45s.
fn nested_scope_budgets_are_ordered() -> bool {
    NESTED_INNER_STEP_S < NESTED_INNER_RUN_S
        && NESTED_INNER_RUN_S < NESTED_OUTER_CHILD_STEP_S
        && NESTED_OUTER_CHILD_STEP_S < NESTED_OUTER_CHILD_RUN_S
        && NESTED_SIGNAL_STEP_S < NESTED_SIGNAL_RUN_S
        && NESTED_SURVIVOR_STEP_S < NESTED_SURVIVOR_RUN_S
        && NESTED_OUTER_CHILD_RUN_S + NESTED_SIGNAL_RUN_S + NESTED_SURVIVOR_RUN_S
            < NESTED_SCOPE_RUNTIME_S
        && NESTED_SCOPE_RUNTIME_S < NESTED_WRAPPER_TIMEOUT_S
}

fn nested_scope_probe_step(
    job: &str,
    cmd: String,
    mode: Option<&str>,
    timeout_s: i64,
) -> dagrun::model::Step {
    let mut step = step_with_caps(
        "safe_ci_scope_self_test", job, "Exercise nested per-step cgroup containment",
        cmd, Vec::new(), timeout_s, timeout_s, 512 * 1024 * 1024,
    );
    if let Some(mode) = mode {
        step.env.insert(NESTED_SCOPE_SELF_TEST_ENV.into(), mode.into());
    }
    step.env.insert("DAGRUN_NO_STEP_LOGS".into(), "1".into());
    step.hint.preferred_inner_jobs = Some(1);
    step.jobs_flag = Some(String::new());
    step
}

/// Total CPU cores the scheduler may hand out across concurrently running steps.
///
/// This is a DIFFERENT quantity from `-j`, which bounds how many steps run at
/// once, and the runner now takes both. Passing `None` defaults the CPU budget to
/// the active-step width, and the runner then refuses before any node starts if a
/// step declares a wider `preferred_inner_jobs` than the budget AND manages its own
/// concurrency (an empty `jobs_flag`) — because clamping such a step's cgroup quota
/// alone would leave its original worker count running inside a smaller box, which
/// is a slowdown disguised as a limit. Four nodes are in exactly that position:
/// `build.workspace` and `build.runtime_release` at 32, and
/// `e2e.manifest_backend_parity_c` and `e2e.manifest_c_programs` at 8. All four
/// bake their measured width into the command itself, so `-j` (host_cpus/8, floor
/// 2, cap 16) would refuse the entire run.
///
/// The value is the one the runner's own CLI defaults to: the ambient
/// container/affinity budget, tightened by the shared aggregate slice's quota. On a
/// host too small to satisfy a declared width the run still refuses, which is the
/// intended fail-closed behavior — the fix there is the step's declaration, not a
/// wider number here.
fn scheduler_cpu_budget() -> i64 {
    container_core_budget().min(aggregate_slice_max_cpus()).max(1)
}

fn run_one_nested_scope_probe_step(
    cgroups: BoxedCgroups,
    step: dagrun::model::Step,
    run_timeout_s: i64,
) -> Result<(), String> {
    let mut cfg = DagConfig {
        description: "real nested safe-ci scope self-test".into(),
        ..Default::default()
    };
    cfg.steps.push(step);
    let result = run_dag_boxed_deadline(
        &cfg, 1, true, 2, cgroups, None, Some(1), Some(run_timeout_s),
    );
    if !result.ok || result.run_timed_out || !result.skipped.is_empty()
        || result.outcomes.len() != 1 || !result.outcomes[0].ok
    {
        return Err(format!(
            "nested cgroup step did not pass exactly once: ok={} timed_out={} outcomes={:?} skipped={:?}",
            result.ok, result.run_timed_out, result.outcomes, result.skipped
        ));
    }
    Ok(())
}

fn run_one_nested_scope_signal_step(
    cgroups: BoxedCgroups,
    step: dagrun::model::Step,
    run_timeout_s: i64,
) -> Result<(), String> {
    let mut cfg = DagConfig {
        description: "real inherited-scope signal self-test".into(),
        ..Default::default()
    };
    cfg.steps.push(step);
    let result = run_dag_boxed_deadline(
        &cfg, 1, true, 2, cgroups, None, Some(1), Some(run_timeout_s),
    );
    if result.ok || result.run_timed_out || !result.skipped.is_empty()
        || result.outcomes.len() != 1
        || result.outcomes[0].ok
        || result.outcomes[0].returncode != Some(-libc::SIGTERM as i64)
    {
        return Err(format!(
            "nested signal step did not fail only by SIGTERM: ok={} timed_out={} \
             outcomes={:?} skipped={:?}",
            result.ok, result.run_timed_out, result.outcomes, result.skipped
        ));
    }
    Ok(())
}

/// Exercise outer-scope verification from a real scheduler `step-*` child,
/// then require that child to dispatch one further per-step cgroup.
fn run_nested_scope_probe() -> Result<String, String> {
    match std::env::var(NESTED_SCOPE_SELF_TEST_ENV).as_deref() {
        Ok(NESTED_SCOPE_OUTER) => {
            let cgroups = safe_ci_scope::resolve_cgroups(
                "safe-ci nested self-test outer", false, Some(NESTED_SCOPE_RUNTIME_S), true,
            ).map_err(|code| format!("outer cgroup setup refused with exit {code}"))?;
            let exe = std::env::current_exe()
                .map_err(|error| format!("cannot resolve self-test executable: {error}"))?;
            let exe = exe.to_str()
                .ok_or_else(|| "self-test executable path is not UTF-8".to_string())?;
            let command = format!("{} --self-test", validate_plan::shell_quote(exe));
            run_one_nested_scope_probe_step(
                cgroups.clone(),
                nested_scope_probe_step(
                    "outer_child", command.clone(), Some(NESTED_SCOPE_INNER),
                    NESTED_OUTER_CHILD_STEP_S,
                ),
                NESTED_OUTER_CHILD_RUN_S,
            )?;
            run_one_nested_scope_signal_step(
                cgroups.clone(),
                nested_scope_probe_step(
                    "signal_child", command, Some(NESTED_SCOPE_SIGNAL), NESTED_SIGNAL_STEP_S,
                ),
                NESTED_SIGNAL_RUN_S,
            )?;
            run_one_nested_scope_probe_step(
                cgroups,
                nested_scope_probe_step(
                    "surviving_sibling", "true".into(), None, NESTED_SURVIVOR_STEP_S,
                ),
                NESTED_SURVIVOR_RUN_S,
            )?;
            Ok(
                "outer scope observed the nested SIGTERM failure and then ran a boxed sibling"
                    .into(),
            )
        }
        Ok(NESTED_SCOPE_INNER) => {
            // This process inherited the outer scope's RuntimeMax; it did not
            // request a second systemd unit. Every other limit stays mandatory.
            let cgroups = safe_ci_scope::resolve_cgroups(
                "safe-ci nested self-test inner", false, None, false,
            ).map_err(|code| format!("nested cgroup setup refused with exit {code}"))?;
            run_one_nested_scope_probe_step(
                cgroups,
                nested_scope_probe_step(
                    "inner_child", "true".into(), None, NESTED_INNER_STEP_S,
                ),
                NESTED_INNER_RUN_S,
            )?;
            Ok("nested child verified the outer scope and dispatched its own boxed step".into())
        }
        Ok(NESTED_SCOPE_SIGNAL) => {
            // Remove validate's ordinary signal observer BEFORE resolve_cgroups.
            // The fixed inherited path installs no replacement, so SIGTERM
            // terminates only this child. The buggy path installs the outer-
            // scope teardown handler, which instead kills the disposable scope
            // and makes the bounded parent wrapper fail.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
            }
            let _cgroups = safe_ci_scope::resolve_cgroups(
                "safe-ci nested signal self-test", false, None, false,
            ).map_err(|code| format!("nested signal cgroup setup refused with exit {code}"))?;
            eprintln!("nested signal child resolved inherited cgroups; delivering SIGTERM");
            let raised = unsafe { libc::raise(libc::SIGTERM) };
            Err(format!("nested signal child survived SIGTERM (libc::raise rc={raised})"))
        }
        Ok(other) => Err(format!("unknown nested scope self-test mode {other:?}")),
        Err(error) => Err(format!("nested scope self-test mode is unavailable: {error}")),
    }
}

/// Launch the real nested topology in a bounded child. Clearing inherited
/// scope sentinels forces that child to establish and observe a fresh scope.
fn nested_scope_self_test() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot resolve self-test executable: {error}"))?;
    let output = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{NESTED_WRAPPER_TIMEOUT_S}s"))
        .arg(exe).arg("--self-test")
        .env(NESTED_SCOPE_SELF_TEST_ENV, NESTED_SCOPE_OUTER)
        .env("DAGRUN_FORCE_SCOPE_ATTEMPT", "1")
        .env("DAGRUN_NO_STEP_LOGS", "1")
        .env_remove("DAGRUN_IN_SCOPE")
        .env_remove("DAGRUN_SCOPE_UNIT")
        .env_remove("DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES")
        .env_remove("DAGRUN_EXPECTED_RUNTIME_MAX_SEC")
        .output()
        .map_err(|error| format!("cannot launch bounded nested scope self-test: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "real nested scope self-test failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status, String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for required in [
        "nested child verified the outer scope and dispatched its own boxed step",
        "safe-ci nested self-test inner: cgroup boxing ACTIVE",
        "outer cgroup audit at ",
        "safe_ci_scope_self_test.inner_child] ✓ PASS",
        "safe_ci_scope_self_test.signal_child] ✗ FAIL",
        "safe_ci_scope_self_test.surviving_sibling] ✓ PASS",
        "outer scope observed the nested SIGTERM failure and then ran a boxed sibling",
    ] {
        if !stdout.contains(required) && !stderr.contains(required) {
            return Err(format!(
                "real nested scope self-test exited successfully without required evidence \
                 {required:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            ));
        }
    }
    Ok("safe-ci scope: real outer -> step child -> nested boxed step passed".into())
}

/// The literal flag that enables Hermit's strict execution mode.
///
/// Spelled out here, in the CONSUMER, on purpose. Deriving it from
/// `CompatMode::run_args` — the code being checked — would make the check agree
/// with whatever that code happens to emit, which is exactly the defect this
/// constant exists to catch: deleting `--strict` from the rendered plans left
/// `--self-test` exiting 0 and printing success, because nothing compared the
/// rendered command against an independently stated expectation.
///
/// This flag alone does NOT establish canonical L2 evidence. The modes checked
/// below use the legacy lossy `--verify` comparator and remain explicitly
/// below-L2 unless their argv also adopts `--verify-strict` and the surrounding
/// evidence policy is updated.
const STRICT_EXECUTION_FLAG: &str = "--strict";

/// Legacy below-L2 compatibility modes whose Hermit-option prefix must carry
/// [`STRICT_EXECUTION_FLAG`].
///
/// `CompatMode::Rr` is deliberately absent rather than overlooked: it renders
/// `record start --verify --verify-strict`, a different path with a different
/// evidence policy and no `--strict` marker, so folding it into this list would
/// assert something untrue about it.
const LEGACY_BELOW_L2_STRICT_MODES: [CompatMode; 4] = [
    CompatMode::Strict,
    CompatMode::PortableStrict,
    CompatMode::Sabre,
    CompatMode::E9patch,
];

/// Whether a rendered plan is missing the literal strict flag from Hermit's
/// option prefix.
///
/// The first `--` ends Hermit's options and begins the guest argv. A guest is
/// free to receive an argument spelled `--strict`; accepting that occurrence as
/// a Hermit option would make the policy check vacuous for exactly the malformed
/// command it is meant to refuse. A missing separator is malformed too.
fn strict_flag_missing_from(argv: &[String]) -> bool {
    let Some(guest_separator) = argv.iter().position(|arg| arg == "--") else {
        return true;
    };
    !argv[..guest_separator]
        .iter()
        .any(|arg| arg == STRICT_EXECUTION_FLAG)
}

/// Exercise the real bootstrap boundary around the first DAG node.
///
/// With agent-utils populated, the Rust driver can start and a missing rr
/// checkout must become a schema-4 FAILED result from `pre.submodules`, even
/// though cgroup setup replaces the process first. With agent-utils absent,
/// rust-script cannot build the driver; that remains a pre-driver bootstrap
/// failure and must not manufacture a typed result.
fn submodule_failure_service_result_bracket(root: &Path) -> Result<String, String> {
    fn run_fixture(checkout: &Path, result: &Path) -> Result<std::process::Output, String> {
        let mut command = Command::new("timeout");
        command
            .args([
                "--signal=TERM",
                "300",
                "./scripts/validate.rs",
                ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION,
                SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                "--only",
                "portable",
                "pre.submodules",
            ])
            .current_dir(checkout)
            .env(VALIDATE_SERVICE_RESULT_PATH_ENV, result)
            .env("DAGRUN_FORCE_SCOPE_ATTEMPT", "1")
            .env_remove("CI")
            .env_remove("GITHUB_ACTIONS")
            .env_remove("DAGRUN_IN_SCOPE")
            .env_remove("DAGRUN_SCOPE_UNIT")
            .env_remove("DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES")
            .env_remove("DAGRUN_EXPECTED_OUTER_CPU_COUNT")
            .env_remove("DAGRUN_EXPECTED_RUNTIME_MAX_SEC")
            .env_remove(OWN_SCOPE_DEADLINE_ENV)
            .env_remove(PARENT_ENV)
            .env_remove(TOOL_ROOT_ENV)
            .env_remove(TOOL_AUTHORITY_ENV)
            .env_remove(TOOL_CONTENT_SHA256_ENV)
            .env_remove(TOOL_PARENT_SHA_ENV)
            .env_remove(TOOL_HERMIT_SHA_ENV)
            .env_remove(TOOL_AGENT_UTILS_SHA_ENV)
            .env_remove(TOOL_BOOTSTRAP_SHA256_ENV)
            .env_remove(validate_runtime::ACTIVE_ENV)
            .env_remove("CI_HUB_VALIDATE_LOCK_OWNER_PID")
            .env_remove("CI_HUB_VALIDATE_LOCK_OWNER_FILE")
            // This fixture deliberately exercises the pre-driver bootstrap in
            // an independent checkout. It must compile that copied driver with
            // the real rust-script so a missing path dependency remains visible
            // instead of consuming this checkout's prepared executable.
            .env_remove("HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED")
            .env_remove("HERMIT_RUST_SCRIPT_ARTIFACT_ROOT");
        command
            .output()
            .map_err(|error| format!("submodule service result: cannot launch fixture: {error}"))
    }

    fn checked_command(command: &mut Command, what: &str) -> Result<(), String> {
        let output = command
            .output()
            .map_err(|error| format!("submodule service result: cannot {what}: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "submodule service result: {what} failed with {:?}: {}{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    let fixture = tempfile::tempdir()
        .map_err(|error| format!("submodule service result: cannot create fixture: {error}"))?;
    let checkout = fixture.path().join("hermit");
    checked_command(
        Command::new("git")
            .args(["clone", "--quiet", "--no-local", "--no-recurse-submodules"])
            .arg(root)
            .arg(&checkout),
        "clone the independent Hermit fixture",
    )?;

    // A self-test can run from an uncommitted edit while it is being developed.
    // Overlay exactly this test's production files, then commit only when that
    // changed the clone. The real child therefore still runs from a clean SHA.
    for relative in [
        ".config/nextest.toml",
        "Makefile",
        "ci/dag/validate.json",
        "ci/manifest-plan/src/runner.rs",
        "ci/manifest-plan/src/service_result.rs",
        "ci/manifest-plan/src/timeouts.rs",
        "ci/manifest-plan/validation-service-result-schema.json",
        "ci/nextest-timeout-config.rs",
        "ci/run-nextest-counted.sh",
        "ci/verify-submodules.sh",
        "scripts/validate.rs",
        "scripts/lib/validate_history.rs",
        "scripts/lib/validate_plan.rs",
        "scripts/lib/validate_super.rs",
        "tests/e2e/manifests/applications.yaml",
        "tests/e2e/manifests/backend-parity-c.yaml",
        "tests/e2e/manifests/c-programs.yaml",
        "tests/e2e/manifests/data-handling.yaml",
        "tests/e2e/manifests/defaults.yaml",
    ] {
        std::fs::copy(root.join(relative), checkout.join(relative)).map_err(|error| {
            format!("submodule service result: cannot copy {relative} into fixture: {error}")
        })?;
    }
    checked_command(
        Command::new("git")
            .args([
                "add",
                "--",
                ".config/nextest.toml",
                "Makefile",
                "ci/dag/validate.json",
                "ci/manifest-plan/src/runner.rs",
                "ci/manifest-plan/src/service_result.rs",
                "ci/manifest-plan/src/timeouts.rs",
                "ci/manifest-plan/validation-service-result-schema.json",
                "ci/nextest-timeout-config.rs",
                "ci/run-nextest-counted.sh",
                "ci/verify-submodules.sh",
                "scripts/validate.rs",
                "scripts/lib/validate_history.rs",
                "scripts/lib/validate_plan.rs",
                "scripts/lib/validate_super.rs",
                "tests/e2e/manifests/applications.yaml",
                "tests/e2e/manifests/backend-parity-c.yaml",
                "tests/e2e/manifests/c-programs.yaml",
                "tests/e2e/manifests/data-handling.yaml",
                "tests/e2e/manifests/defaults.yaml",
            ])
            .current_dir(&checkout),
        "stage the fixture sources",
    )?;
    let staged = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(&checkout)
        .status()
        .map_err(|error| {
            format!("submodule service result: cannot inspect fixture diff: {error}")
        })?;
    if !staged.success() {
        checked_command(
            Command::new("git")
                .args([
                    "-c",
                    "user.name=validate fixture",
                    "-c",
                    "user.email=validate-fixture@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "validate service-result fixture",
                ])
                .current_dir(&checkout),
            "commit the fixture sources",
        )?;
    }

    let bootstrap_result = fixture.path().join("bootstrap-result.json");
    let bootstrap = run_fixture(&checkout, &bootstrap_result)?;
    let bootstrap_output = format!(
        "{}{}",
        String::from_utf8_lossy(&bootstrap.stdout),
        String::from_utf8_lossy(&bootstrap.stderr)
    );
    if bootstrap.status.success()
        || bootstrap_result.exists()
        || String::from_utf8_lossy(&bootstrap.stdout).contains(FINAL_VALIDATE_STATUS_PREFIX)
        || !bootstrap_output.contains("agent-utils")
    {
        return Err(format!(
            "submodule service result: missing agent-utils did not remain a diagnosed bootstrap failure: \
             status={:?} result_exists={} output={bootstrap_output}",
            bootstrap.status.code(),
            bootstrap_result.exists()
        ));
    }

    checked_command(
        Command::new("git")
            .args(["clone", "--quiet", "--no-local"])
            .arg(root.join("agent-utils"))
            .arg(checkout.join("agent-utils")),
        "populate only agent-utils",
    )?;
    let expected_agent_utils = Command::new("git")
        .args(["ls-tree", "HEAD", "agent-utils"])
        .current_dir(&checkout)
        .output()
        .map_err(|error| {
            format!("submodule service result: cannot read agent-utils pin: {error}")
        })?;
    let expected_agent_utils = String::from_utf8_lossy(&expected_agent_utils.stdout)
        .split_whitespace()
        .nth(2)
        .ok_or("submodule service result: agent-utils gitlink is absent")?
        .to_string();
    checked_command(
        Command::new("git")
            .args(["checkout", "--quiet", &expected_agent_utils])
            .current_dir(checkout.join("agent-utils")),
        "checkout the recorded agent-utils pin",
    )?;

    let result_path = fixture.path().join("service-result.json");
    let output = run_fixture(&checkout, &result_path)?;
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result = ValidationServiceResult::from_json_slice(&std::fs::read(&result_path).map_err(
        |error| {
            format!(
                "submodule service result: pre.submodules failure wrote no typed result: {error}; \
                 status={:?} output={rendered}",
                output.status.code()
            )
        },
    )?)?;
    if output.status.code() != Some(1)
        || result.final_validate_status != FinalValidateStatus::Failed
        || result.exit_code != 1
        || result.executed_nodes != 1
        || !rendered.contains("pre.submodules")
        || !rendered.contains("third-party/rr")
        || !rendered.contains("FINAL_VALIDATE_STATUS: FAILED")
    {
        return Err(format!(
            "submodule service result: missing rr was not attributed to the real first DAG node: \
             status={:?} result={result:?} output={rendered}",
            output.status.code()
        ));
    }

    Ok("missing agent-utils stays a bootstrap failure; with agent-utils present, missing rr is a typed pre.submodules failure across scope re-exec".into())
}

/// Pin the measured resource policy for the shard-coverage guard.
///
/// Two 512 MiB runs were OOM-killed. An uncontended cold run completed in
/// 269.033s with a 780.7 MiB peak, so the old 60s/512 MiB bounds could not
/// contain the command they claimed to guard. The 64 MiB estimate remains the
/// warm-cache scheduling estimate; it is not the hard safety ceiling.
fn shard_coverage_resource_policy_bracket(root: &Path) -> Result<(), String> {
    const MIB: i64 = 1024 * 1024;
    const EXPECTED: (i64, Option<i64>, Option<i64>) =
        (600, Some(64 * MIB), Some(1024 * MIB));
    fn policy(step: &Step) -> (i64, Option<i64>, Option<i64>) {
        (
            step.timeout,
            step.hint.rss_baseline_bytes,
            step.hint.hard_mem_max_bytes,
        )
    }
    let cfg = validate_plan::lane_config(root, "portable")?;
    let mut matches = cfg.steps.iter().filter(|step| step.tag() == "check.shard_coverage");
    let shipped = matches
        .next()
        .ok_or("shard-coverage resource policy: portable DAG lost check.shard_coverage")?;
    if matches.next().is_some() {
        return Err("shard-coverage resource policy: duplicate check.shard_coverage nodes".into());
    }
    if !shipped.cmd.ends_with("./ci/check-shard-coverage.sh") || policy(shipped) != EXPECTED {
        return Err(format!(
            "shard-coverage resource policy changed: got cmd={:?} policy={:?}, expected the canonical suffix and {EXPECTED:?}",
            shipped.cmd,
            policy(shipped),
        ));
    }

    let mut old_timeout = shipped.clone();
    old_timeout.timeout = 60;
    let mut old_cap = shipped.clone();
    old_cap.hint.hard_mem_max_bytes = Some(512 * MIB);
    for (name, mutated) in [("60s timeout", old_timeout), ("512 MiB hard cap", old_cap)] {
        if policy(&mutated) == EXPECTED {
            return Err(format!("shard-coverage resource policy: planted {name} was accepted"));
        }
    }

    println!(
        "  shard coverage: old 60s/512 MiB bounds refused; 600s/1 GiB hard bounds retain the 64 MiB warm estimate"
    );
    Ok(())
}

/// Inert brackets for the policy predicate and the shell quoter.
///
/// These cannot launch a run or authorize a receipt — they only prove the
/// predicate refuses every non-qualifying case AND accepts the one qualifying
/// case, so it is not vacuously true. `validate.sh` ran the equivalent brackets
/// on every invocation (validate.sh:308); here they are a `--self-test` subcommand
/// so the cost is not paid on the hot path.
fn self_test() -> Result<(), String> {
    inner_freshness_skip_cli_bracket()?;
    run_owned_cache_bracket()?;
    run_state_path_bracket()?;
    println!("  {}", committed_validation_execution_bracket(&repo_root())?);
    println!("  {}", raw_run_dag_strict_compat_bracket(&repo_root())?);
    println!("  {}", raw_run_dag_engine_bracket(&repo_root())?);
    shard_coverage_resource_policy_bracket(&repo_root())?;
    println!(
        "  {}",
        submodule_failure_service_result_bracket(&repo_root())?
    );

    // ---- known-fail-closed disposition, as a pure decision table ----
    //
    // The property under test is NOT "the listed rows get mentioned". It is that mentioning
    // them did not quietly excuse them. Each row below pins BOTH the message class and the
    // blocking verdict, so a future edit cannot improve the wording into an exemption.
    {
        use validate_plan::CompatDisposition as D;
        use validate_plan::classify_compat_outcome as classify;
        // (mode, ok, listed_failclosed, listed_diagnostic, expected, expected_blocking)
        let cases: &[(CompatMode, bool, bool, bool, D, bool)] = &[
            // PortableStrict: the whole point of this change. A listed failure is REPORTED and
            // STILL BLOCKS; a listed pass is reported as a stale expectation.
            (CompatMode::PortableStrict, false, true, false, D::KnownFailClosedBlocking, true),
            (CompatMode::PortableStrict, true, true, false, D::PassedButListedFailClosed, false),
            // ...and an UNLISTED failure is unaffected.
            (CompatMode::PortableStrict, false, false, false, D::Blocking, true),
            // Bounded portable diagnostics keep their existing nonblocking treatment.
            (CompatMode::PortableStrict, false, false, true, D::PortableDiagnostic, false),
            // Strict keeps its historical exemption, and only Strict has it.
            (CompatMode::Strict, false, true, false, D::KnownFailClosedExempt, false),
            (CompatMode::Strict, true, true, false, D::PassedButListedFailClosed, false),
            (CompatMode::Strict, false, false, false, D::Blocking, true),
            // No other mode consults either table: a failure blocks whatever the tables say.
            (CompatMode::Sabre, false, true, false, D::Blocking, true),
            (CompatMode::Sabre, false, false, true, D::Blocking, true),
            (CompatMode::Sabre, true, true, false, D::Passed, false),
            (CompatMode::E9patch, false, true, false, D::Blocking, true),
            (CompatMode::Rr, false, true, false, D::Blocking, true),
        ];
        for (mode, ok, listed, diag, want, want_blocking) in cases.iter().copied() {
            let got = classify(mode, ok, listed, diag);
            if got != want {
                return Err(format!(
                    "compat disposition for mode={mode:?} ok={ok} listed_failclosed={listed} \
                     listed_diagnostic={diag}: expected {want:?}, got {got:?}"
                ));
            }
            if got.is_blocking() != want_blocking {
                return Err(format!(
                    "compat disposition {got:?} (mode={mode:?} ok={ok} listed={listed}) must \
                     {} the run, but is_blocking() said {}",
                    if want_blocking { "BLOCK" } else { "not block" },
                    got.is_blocking()
                ));
            }
        }
    }

    // ---- the shipped portable-diagnostic table, not a planted substitute ----
    //
    // RUN 1622 established the exact boundary this table must express: ranlib's
    // functional assertion completed, while its below-L2 directory-payload
    // verification diverged. That one row is diagnostic under PortableStrict;
    // its neighboring ordinary corpus row remains blocking. Pin the full table
    // so this exception cannot silently broaden.
    {
        use validate_plan::CompatDisposition as D;
        use validate_plan::classify_compat_outcome as classify;

        let diagnostic = validate_corpus::portable_diagnostic();
        let labels: BTreeSet<&str> = diagnostic.keys().copied().collect();
        let expected = BTreeSet::from(["df", "ranlib", "top", "zstd", "zstd-roundtrip"]);
        if labels != expected {
            return Err(format!(
                "portable compatibility diagnostic set changed: got {labels:?}, expected {expected:?}"
            ));
        }
        let ranlib = classify(
            CompatMode::PortableStrict,
            false,
            false,
            diagnostic.contains_key("ranlib"),
        );
        if ranlib != D::PortableDiagnostic
            || ranlib.is_blocking()
            || CompatMode::PortableStrict.timeout_for("ranlib") != 20
        {
            return Err(format!(
                "compat.ranlib must remain a bounded nonblocking PortableStrict diagnostic: disposition={ranlib:?}, timeout={}s",
                CompatMode::PortableStrict.timeout_for("ranlib")
            ));
        }
        let ordinary = classify(
            CompatMode::PortableStrict,
            false,
            false,
            diagnostic.contains_key("readelf"),
        );
        if ordinary != D::Blocking
            || !ordinary.is_blocking()
            || CompatMode::PortableStrict.timeout_for("readelf") != 60
        {
            return Err(format!(
                "ordinary compat.readelf failure stopped blocking: disposition={ordinary:?}, timeout={}s",
                CompatMode::PortableStrict.timeout_for("readelf")
            ));
        }
    }

    // ---- the REAL summary consumer, against a PLANTED table ----
    //
    // Bound to the shipped `compat_summary_with_tables`, not a copy of its logic, so the two
    // cannot drift. The table is planted rather than real because the shipped
    // `known_failclosed()` holds ONE row today, which cannot express "one listed row blocks
    // while another listed row is exempt" in a single run -- and the answer to that is a
    // planted table in the bracket, never an invented row in production.
    {
        let planted_known: BTreeMap<&'static str, &'static str> = BTreeMap::from([
            ("listed_fails", "planted: refused by fail-closed --strict"),
            ("listed_passes", "planted: expected to be refused"),
        ]);
        let planted_diag: BTreeMap<&'static str, &'static str> =
            BTreeMap::from([("bounded_diag", "planted: bounded portable diagnostic")]);
        let row = |label: &str, ok: bool| StepOutcome {
            tag: format!("compat.{label}"),
            ok,
            duration_s: 0.0,
            summary: String::new(),
            executed_tests: None,
            filtered_tests: None,
            test_results: None,
            returncode: Some(if ok { 0 } else { 1 }),
            oomed: false,
            oom_kills: 0,
            timed_out: false,
            cpu_timed_out: false,
            reason: String::new(),
            aborted: false,
        };
        let outcomes = vec![
            row("listed_fails", false),
            row("listed_passes", true),
            row("bounded_diag", false),
            row("unlisted_fails", false),
            row("plain_passes", true),
        ];
        let (passed, measured, blocking, nonblocking) = compat_summary_with_tables(
            CompatMode::PortableStrict,
            "compat.",
            &outcomes,
            &planted_known,
            &planted_diag,
        );
        if (passed, measured) != (2, 5) {
            return Err(format!(
                "compatibility measurement: completed fixture population reported {passed}/{measured}, want 2/5"
            ));
        }
        // THE LOAD-BEARING ASSERTION: a listed failure is still in the blocking set. If a future
        // change makes PortableStrict exempt listed rows the way Strict does, this fails.
        if !blocking.iter().any(|l| l == "listed_fails") {
            return Err(format!(
                "PortableStrict dropped a listed known-fail-closed row from the blocking set, \
                 which is the exemption this change exists to avoid: blocking={blocking:?}"
            ));
        }
        if !blocking.iter().any(|l| l == "unlisted_fails") {
            return Err(format!(
                "PortableStrict dropped an UNLISTED failure from the blocking set: \
                 blocking={blocking:?}"
            ));
        }
        if blocking.iter().any(|l| l == "bounded_diag") {
            return Err(format!(
                "a bounded portable diagnostic became blocking, changing prior policy: \
                 blocking={blocking:?}"
            ));
        }
        if nonblocking != BTreeSet::from(["compat.bounded_diag".to_string()]) {
            return Err(format!(
                "PortableStrict nonblocking failures did not come from the same typed disposition as the verdict: {nonblocking:?}"
            ));
        }
        if blocking.iter().any(|l| l == "listed_passes" || l == "plain_passes") {
            return Err(format!("a PASSING row was reported as blocking: blocking={blocking:?}"));
        }

        // A scheduler record is not evidence that the program ran. Spawn and
        // supervisor failures have no child exit status, while an aborted row
        // was stopped before producing a verdict. Neither may change the
        // measured denominator or gain a compatibility failure classification.
        let mut unknown = row("unknown_execution", false);
        unknown.returncode = None;
        let mut aborted = row("aborted_execution", false);
        aborted.aborted = true;
        let mut with_unknown = outcomes.clone();
        with_unknown.extend([unknown, aborted]);
        let (unknown_passed, unknown_measured, unknown_blocking, unknown_nonblocking) =
            compat_summary_with_tables(
                CompatMode::PortableStrict,
                "compat.",
                &with_unknown,
                &planted_known,
                &planted_diag,
            );
        if (unknown_passed, unknown_measured) != (passed, measured)
            || unknown_blocking != blocking
            || unknown_nonblocking != nonblocking
        {
            return Err(format!(
                "compatibility measurement: unknown_execution or aborted_execution changed the \
                 measured population: base={passed}/{measured} {blocking:?}, with unknown={unknown_passed}/{unknown_measured} {unknown_blocking:?}"
            ));
        }
        // And the same planted table under Strict must exempt the listed failure, so the
        // bracket also pins that the two modes still differ.
        let (_, _, strict_blocking, strict_nonblocking) = compat_summary_with_tables(
            CompatMode::Strict,
            "compat.",
            &outcomes,
            &planted_known,
            &planted_diag,
        );
        if strict_blocking.iter().any(|l| l == "listed_fails") {
            return Err(format!(
                "Strict lost its historical exemption for a listed row: {strict_blocking:?}"
            ));
        }
        if strict_nonblocking != BTreeSet::from(["compat.listed_fails".to_string()]) {
            return Err(format!(
                "Strict nonblocking failures did not retain exactly its listed exemption: {strict_nonblocking:?}"
            ));
        }
    }


    // Strict-execution bracket for the legacy below-L2 compatibility modes.
    // `--strict` must be a Hermit option before the first guest `--`; these
    // modes still use lossy `--verify`, so this does not call them L2. The
    // bracket accepts the real prefix, rejects deletion, and rejects the subtle
    // goalpost move of putting the same spelling in guest argv.
    for mode in LEGACY_BELOW_L2_STRICT_MODES {
        let rendered = mode.run_args("whoami", "/tmp/nsswitch.conf");
        let guest_separator = rendered
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| format!("{mode:?} rendered no guest argv separator: {rendered:?}"))?;
        if strict_flag_missing_from(&rendered) {
            return Err(format!(
                "{mode:?} rendered a compatibility plan WITHOUT the literal \
                 {STRICT_EXECUTION_FLAG} in Hermit's option prefix, so the \
                 legacy below-L2 run would not use strict execution: {rendered:?}"
            ));
        }
        if matches!(mode, CompatMode::Strict | CompatMode::PortableStrict)
            && !rendered[..guest_separator]
                .windows(2)
                .any(|pair| pair == ["--env", "TMPDIR=/tmp"])
        {
            return Err(format!(
                "{mode:?} did not override the inherited host TMPDIR with the guest-visible \
                 /tmp before the guest argv separator: {rendered:?}"
            ));
        }
        if rendered[..guest_separator]
            .iter()
            .any(|arg| arg == "--verify-strict")
            || !mode.display_name().contains("below-L2")
        {
            return Err(format!(
                "{mode:?} is governed by the legacy below-L2 policy, so its Hermit prefix must \
                 omit --verify-strict and its rendered description must say below-L2: \
                 argv={rendered:?}, description={:?}",
                mode.display_name()
            ));
        }
        let stripped: Vec<String> = rendered
            .iter()
            .filter(|arg| *arg != STRICT_EXECUTION_FLAG)
            .cloned()
            .collect();
        if stripped.len() == rendered.len() {
            return Err(format!(
                "{mode:?}: removing {STRICT_EXECUTION_FLAG} changed nothing, so the \
                 refusing direction below would be vacuous"
            ));
        }
        if !strict_flag_missing_from(&stripped) {
            return Err(format!(
                "the strict-flag check did not notice {STRICT_EXECUTION_FLAG} missing \
                 from a {mode:?} plan, so it cannot detect its deletion"
            ));
        }
        let stripped_separator = stripped
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| format!("{mode:?} rendered no guest argv separator: {rendered:?}"))?;
        let mut misplaced = stripped.clone();
        misplaced.insert(stripped_separator + 1, STRICT_EXECUTION_FLAG.into());
        if !strict_flag_missing_from(&misplaced) {
            return Err(format!(
                "the strict-flag check accepted {STRICT_EXECUTION_FLAG} after the first guest \
                 separator in a {mode:?} plan, so guest argv could forge the Hermit option: \
                 {misplaced:?}"
            ));
        }
        let no_separator: Vec<String> = rendered
            .iter()
            .filter(|arg| arg.as_str() != "--")
            .cloned()
            .collect();
        if !strict_flag_missing_from(&no_separator) {
            return Err(format!(
                "the strict-flag check accepted a {mode:?} plan with no guest argv separator: \
                 {no_separator:?}"
            ));
        }
    }

    let final_command = "  tail -F -- $'/tmp/holder run.log'".to_string();
    let mut refusal = RunSummary::refused(
        3,
        "self-test",
        "the per-checkout invocation lock",
        vec!["another validate is already running".into()],
    )
    .with_epilogue(vec![
        "watch the holder's live log with:".into(),
        final_command.clone(),
    ]);
    refusal.cpu_wall = Some((1.0, 0.1, 0.1));
    let rendered = run_summary_lines(&refusal, std::time::Instant::now());
    let final_status = format!("{FINAL_VALIDATE_STATUS_PREFIX}COULD_NOT_RUN");
    if rendered.last() != Some(&final_status)
        || rendered.get(rendered.len().saturating_sub(2)) != Some(&final_command)
        || rendered
            .iter()
            .filter(|line| line.starts_with(FINAL_VALIDATE_STATUS_PREFIX))
            .count()
            != 1
    {
        return Err(format!(
            "summary: refusal must end with exactly one final status after the holder command, got {:?}",
            rendered
        ));
    }
    let quoted = format!(
        "{FINAL_VALIDATE_STATUS_PREFIX}PASSED\nforeign output\n{FINAL_VALIDATE_STATUS_PREFIX}FAILED"
    );
    if final_validate_status_from_output(&quoted) != Ok(Some(FinalValidateStatus::Failed))
        || final_validate_status_from_output("ordinary output") != Ok(None)
        || final_validate_status_from_output("FINAL_VALIDATE_STATUS: MAYBE").is_ok()
    {
        return Err(
            "summary: final-status reader did not take the last occurrence, preserve absence, or reject an unknown value"
                .into(),
        );
    }
    for (verdict, word, exit_code) in [
        (Verdict::Pass, "PASSED", 0),
        (Verdict::Fail, "FAILED", 1),
        (Verdict::NoResult, "COULD_NOT_RUN", COULD_NOT_RUN_EXIT_CODE),
    ] {
        let summary = RunSummary::new(verdict, 222, "self-test", Vec::new());
        let lines = run_summary_lines(&summary, std::time::Instant::now());
        let expected = format!("{FINAL_VALIDATE_STATUS_PREFIX}{word}");
        if summary.exit_code != exit_code
            || lines.last().map(String::as_str) != Some(expected.as_str())
        {
            return Err(format!(
                "summary: {word} did not use fixed exit {exit_code} and the matching final line: exit={} lines={lines:?}",
                summary.exit_code
            ));
        }
    }
    let mut writeback_failed = RunSummary::new(Verdict::Pass, 0, "self-test", Vec::new());
    writeback_failed.executed_tests = Some(1);
    writeback_failed.passed_tests = Some(1);
    record_scorecard_writeback(&mut writeback_failed, Some(Err("fixture refusal".into())));
    let lines = run_summary_lines(&writeback_failed, std::time::Instant::now());
    if (writeback_failed.verdict, writeback_failed.exit_code)
        != (Verdict::Pass, COULD_NOT_RUN_EXIT_CODE)
        || lines.last().map(String::as_str) != Some("FINAL_VALIDATE_STATUS: PASSED")
        || !lines.iter().any(|line| line.contains("validation verdict above is unchanged"))
    {
        return Err(format!(
            "summary: a required scorecard write-back failure did not preserve the validation \
             verdict, fail the command distinctly, and remain before the final status: \
             verdict={:?} command_exit={} lines={lines:?}",
            writeback_failed.verdict,
            writeback_failed.exit_code,
        ));
    }
    let writeback_result_dir = tempfile::tempdir()
        .map_err(|error| format!("summary: cannot create write-back result fixture: {error}"))?;
    let writeback_result_path = writeback_result_dir.path().join("result.json");
    write_validation_service_result(&writeback_result_path, &writeback_failed)?;
    let writeback_result = ValidationServiceResult::from_json_slice(
        &std::fs::read(&writeback_result_path)
            .map_err(|error| format!("summary: cannot read write-back result: {error}"))?,
    )?;
    if writeback_result.final_validate_status != FinalValidateStatus::Passed
        || writeback_result.exit_code != i32::from(COULD_NOT_RUN_EXIT_CODE)
        || writeback_result.scorecard_writeback
            != Some(ScorecardWriteback::Failed {
                error: "fixture refusal".into(),
            })
    {
        return Err(format!(
            "summary: scorecard write-back refusal did not preserve the validation verdict and carry its own typed failure: {writeback_result:?}"
        ));
    }
    let mut genuine_could_not_run =
        RunSummary::new(Verdict::NoResult, COULD_NOT_RUN_EXIT_CODE, "self-test", Vec::new());
    genuine_could_not_run.nodes_executed = 1;
    let could_not_run_path = writeback_result_dir.path().join("could-not-run.json");
    write_validation_service_result(&could_not_run_path, &genuine_could_not_run)?;
    let could_not_run = ValidationServiceResult::from_json_slice(
        &std::fs::read(&could_not_run_path)
            .map_err(|error| format!("summary: cannot read could-not-run result: {error}"))?,
    )?;
    if could_not_run.final_validate_status != FinalValidateStatus::CouldNotRun
        || could_not_run.exit_code != i32::from(COULD_NOT_RUN_EXIT_CODE)
        || could_not_run.scorecard_writeback.is_some()
        || run_summary_lines(&genuine_could_not_run, std::time::Instant::now())
            .last()
            .map(String::as_str)
            != Some("FINAL_VALIDATE_STATUS: COULD_NOT_RUN")
    {
        return Err(format!(
            "summary: genuine could-not-run collapsed into a write-back failure: {could_not_run:?}"
        ));
    }
    let service_result_dir = tempfile::tempdir()
        .map_err(|error| format!("summary: cannot create service-result fixture: {error}"))?;
    let service_result_path = service_result_dir.path().join("result.json");
    let mut service_summary = RunSummary::new(Verdict::Pass, 0, "full", Vec::new());
    service_summary.nodes_executed = 76;
    service_summary.executed_tests = Some(2129);
    service_summary.passed_tests = Some(2129);
    write_validation_service_result(&service_result_path, &service_summary)?;
    let service_result = ValidationServiceResult::from_json_slice(
        &std::fs::read(&service_result_path)
            .map_err(|error| format!("summary: cannot read service result: {error}"))?,
    )?;
    if service_result.final_validate_status != FinalValidateStatus::Passed
        || service_result.exit_code != 0
        || service_result.executed_nodes != 76
        || service_result.executed_tests != Some(2129)
        || service_result.passed_tests != Some(2129)
        || service_result.scorecard_writeback.is_some()
    {
        return Err(format!(
            "summary: framework service result lost typed status or counts: {service_result:?}"
        ));
    }

    let cache_tree = "b".repeat(40);
    let cache_key = validate_history::CacheKey {
        tree: &cache_tree,
        profile: "full",
        host: "fixture-host",
        toolchain: "fixture-toolchain",
    };
    let cache_row = serde_json::json!({
        "schema_version": validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION,
        "tree": cache_tree,
        "profile": "full",
        "host": "fixture-host",
        "toolchain": "fixture-toolchain",
        "selection_mode": "full",
        "result": "pass",
        "commit_anchored": true,
        "tree_dirty": false,
        "failures": 0,
        "commit": "b".repeat(40),
        "finished_at": "2026-09-05T00:00:00Z",
        "real_seconds": 100.0,
        "user_seconds": 30.0,
        "sys_seconds": 2.0,
        "producer": "hermit-validate-rs",
        "executed_nodes": 76,
        "executed_tests": 2129,
        "passed_tests": 2129,
        "gates_expected": 76,
        "gates_run": 76,
        "coverage": {
            "planned_test_nodes": 20,
            "executed_test_nodes": 20,
            "absent_nodes": [],
        },
    });
    let cache_summary = |row: &serde_json::Value| -> Result<RunSummary, String> {
        let hit = validate_history::cache_lookup(
            std::slice::from_ref(row),
            "pass",
            &cache_key,
        )
        .ok_or_else(|| "summary: planted cache row did not produce a cache hit".to_string())?;
        cache_hit_run_summary(
            &hit,
            "full",
            "full",
            &cache_tree,
            Path::new("fixture-ledger"),
            true,
        )
    };
    let cached_result_path = service_result_dir.path().join("cache-hit.json");
    write_validation_service_result(&cached_result_path, &cache_summary(&cache_row)?)?;
    let cached_result = ValidationServiceResult::from_json_slice(
        &std::fs::read(&cached_result_path)
            .map_err(|error| format!("summary: cannot read cache-hit result: {error}"))?,
    )?;
    if cached_result.final_validate_status != FinalValidateStatus::Passed
        || cached_result.executed_nodes != 76
        || cached_result.executed_tests != Some(2129)
        || cached_result.passed_tests != Some(2129)
    {
        return Err(format!(
            "summary: exact cache hit did not survive the production service-result writer: {cached_result:?}"
        ));
    }
    for (label, replacement) in [
        ("missing", serde_json::Value::Null),
        ("negative", serde_json::json!(-1)),
        ("contradictory", serde_json::json!(2128)),
    ] {
        let mut row = cache_row.clone();
        if label == "missing" {
            row.as_object_mut().unwrap().remove("passed_tests");
        } else {
            row["passed_tests"] = replacement;
        }
        let refusal = cache_summary(&row).err().ok_or_else(|| {
            format!("summary: {label} cached passed_tests produced a service-result PASS")
        })?;
        if !refusal.contains("cannot publish a current validation service result") {
            return Err(format!("summary: {label} cached count refusal was unclear: {refusal}"));
        }
    }
    let mut legacy_cache_row = cache_row.clone();
    legacy_cache_row["schema_version"] = serde_json::json!(
        validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION - 1
    );
    if cache_summary(&legacy_cache_row).is_ok() {
        return Err("summary: a legacy cache row produced a current service-result PASS".into());
    }

    service_summary.passed_tests = Some(2128);
    let mismatch_path = service_result_dir.path().join("mismatch.json");
    let mismatch_error = write_validation_service_result(&mismatch_path, &service_summary)
        .expect_err("a successful service result must reject a mismatched passed count");
    if !mismatch_error.contains("requires passed_tests == executed_tests") {
        return Err(format!(
            "summary: service result did not refuse a mismatched passed count: {mismatch_error}"
        ));
    }
    service_summary.passed_tests = Some(2129);
    let overwrite_error = write_validation_service_result(&service_result_path, &service_summary)
        .expect_err("a second writer must not replace the first service result");
    if !overwrite_error.contains("without replacing an existing result") {
        return Err(format!(
            "summary: service-result collision did not fail by name: {overwrite_error}"
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|error| format!("summary: cannot resolve self-test executable: {error}"))?;
    let output = Command::new(exe)
        .arg("--self-test")
        .env(SUMMARY_EPILOGUE_SELF_TEST_ENV, "1")
        .output()
        .map_err(|error| format!("summary: cannot launch CLI output probe: {error}"))?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("summary: CLI output was not UTF-8: {error}"))?;
    if output.status.code() != Some(i32::from(COULD_NOT_RUN_EXIT_CODE))
        || stdout.lines().last() != Some(final_status.as_str())
        || stdout.lines().rev().nth(1) != Some(final_command.as_str())
    {
        return Err(format!(
            "summary: real refused CLI must exit {COULD_NOT_RUN_EXIT_CODE}, preserve the holder command, and end with {final_status:?}; status={} last={:?}",
            output.status,
            stdout.lines().last()
        ));
    }

    if !nested_scope_probe_selected(true, true)
        || nested_scope_probe_selected(false, true)
        || nested_scope_probe_selected(true, false)
        || nested_scope_probe_selected(false, false)
    {
        return Err(
            "nested scope probe dispatch did not require both --self-test and its internal marker"
                .into(),
        );
    }
    if !nested_scope_budgets_are_ordered() {
        return Err(format!(
            "nested scope self-test budgets are inverted: inner={NESTED_INNER_STEP_S}/\
             {NESTED_INNER_RUN_S}s outer-child={NESTED_OUTER_CHILD_STEP_S}/\
             {NESTED_OUTER_CHILD_RUN_S}s signal={NESTED_SIGNAL_STEP_S}/\
             {NESTED_SIGNAL_RUN_S}s survivor={NESTED_SURVIVOR_STEP_S}/\
             {NESTED_SURVIVOR_RUN_S}s scope={NESTED_SCOPE_RUNTIME_S}s \
             wrapper={NESTED_WRAPPER_TIMEOUT_S}s"
        ));
    }
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
    if effective_run_timeout(None, Some(1619), true).is_some() {
        return Err(
            "plan-only invocation inherited an enclosing validation's execution timeout".into(),
        );
    }
    if effective_run_timeout(Some(600), Some(1619), true) != Some(600)
        || effective_run_timeout(None, Some(1619), false) != Some(1619)
        || effective_run_timeout(Some(600), Some(1619), false) != Some(600)
    {
        return Err(
            "explicit plan audit or real execution lost its timeout selection semantics".into(),
        );
    }
    if scope_grace_s(600) != 60 || 600 + scope_grace_s(600) >= 720 {
        return Err("run-timeout scope backstop no longer satisfies 600 < 660 < 720".into());
    }
    // A node the scheduler NAMED in `not_launched` is accounted for; one it did not
    // name is a mystery. `not_launched` is neutral: dagrun also uses it when the
    // outer run budget expires, not only when fail-fast stops admission.
    {
        let unreported = vec![
            "e2e.manifest_applications".to_string(),
            "test.detcore_misc".to_string(),
        ];
        let named: BTreeSet<String> = ["e2e.manifest_applications".to_string()]
            .into_iter()
            .collect();
        let (skipped, unaccounted) = partition_unreported(&unreported, &named);
        if skipped != vec!["e2e.manifest_applications".to_string()] {
            return Err(format!(
                "a node named in not_launched must read as an accounted-for scheduler not-launched result, \
                 got {skipped:?}"
            ));
        }
        if unaccounted != vec!["test.detcore_misc".to_string()] {
            return Err(format!(
                "a node absent from not_launched must stay UNACCOUNTED FOR, got {unaccounted:?}"
            ));
        }
        if skipped.len() + unaccounted.len() != unreported.len() {
            return Err("partitioning unreported nodes must not drop any of them".into());
        }
        // The pre-fix behaviour: with nothing named, every node is still a mystery.
        let (none_named, all_unaccounted) = partition_unreported(&unreported, &BTreeSet::new());
        if !none_named.is_empty() || all_unaccounted.len() != 2 {
            return Err(
                "an empty not_launched must leave every unreported node unaccounted for".into(),
            );
        }
        // ...UNLESS the lane was refused before launching. A refusal empties all
        // four scheduler collections on purpose, so without this third state a
        // refusal would report every planned node as unaccounted for directly below
        // a refusal that states the reason -- the unaccounted signal failing exactly when loudest.
        if !scheduler_refused_before_launching(193, 0, 0, 0, 0) {
            return Err(
                "a lane that produced no outcome, dependency-skip, fail-fast skip or intentional \
                 skip must be recognised as refused before launching"
                    .into(),
            );
        }
        // A lane that ran and merely lost nodes is NOT a refusal, and must keep
        // reporting them as unaccounted for.
        if scheduler_refused_before_launching(54, 40, 0, 0, 0)
            || scheduler_refused_before_launching(54, 0, 7, 0, 0)
            || scheduler_refused_before_launching(54, 0, 0, 7, 0)
            || scheduler_refused_before_launching(54, 0, 0, 0, 7)
        {
            return Err(
                "a lane that produced outcomes, skips or not-launched entries is not a pre-flight \
                 refusal and must not be excused as one"
                    .into(),
            );
        }
        if scheduler_refused_before_launching(0, 0, 0, 0, 0) {
            return Err("an empty plan is not a refusal".into());
        }

        // Explanations describe the latest attempt, not an accumulated history. A
        // not-launched result followed by a retry refusal must read as refused; a later
        // completed retry must clear both non-run explanations.
        let retry_tag = "e2e.manifest_applications".to_string();
        let planned = vec![retry_tag.clone()];
        let mut latest_not_launched = BTreeSet::new();
        let mut latest_refused = BTreeSet::new();
        update_not_run_explanations(
            &planned,
            0,
            0,
            std::slice::from_ref(&retry_tag),
            0,
            &mut latest_not_launched,
            &mut latest_refused,
        );
        if !latest_not_launched.contains(&retry_tag) || latest_refused.contains(&retry_tag) {
            return Err("a scheduler not-launched result must be recorded for the latest attempt".into());
        }
        update_not_run_explanations(
            &planned,
            0,
            0,
            &[],
            0,
            &mut latest_not_launched,
            &mut latest_refused,
        );
        if latest_not_launched.contains(&retry_tag) || !latest_refused.contains(&retry_tag) {
            return Err(
                "a retry refusal must replace the earlier not-launched explanation".into(),
            );
        }
        update_not_run_explanations(
            &planned,
            1,
            0,
            &[],
            0,
            &mut latest_not_launched,
            &mut latest_refused,
        );
        if latest_not_launched.contains(&retry_tag) || latest_refused.contains(&retry_tag) {
            return Err("a completed retry must clear every non-run explanation".into());
        }
    }
    // The ceiling must stay strictly inside the budget the scheduler enforces at
    // EVERY remainder, not only at the nominal one. 441s is the measured remainder
    // that refused the strict-compat lane on 2026-08-25 while its gate ceiling was
    // a fixed 480s; 8s is the largest wall those nodes actually needed.
    // A 1s remainder admits no ceiling that is both usable and strictly smaller;
    // the epoch is over and the scheduler refusing is then correct.
    for remaining in [441_i64, 480, 600, 30, 2] {
        let ceiling = derived_wall_ceiling(remaining);
        if ceiling >= remaining {
            return Err(format!(
                "derived wall ceiling {ceiling}s does not fit inside a {remaining}s remainder, so \
                 the scheduler would refuse the lane instead of running it"
            ));
        }
        if ceiling < 1 {
            return Err(format!("derived wall ceiling {ceiling}s is not a usable budget"));
        }
    }
    if derived_wall_ceiling(441) >= 480 {
        return Err(
            "a 441s remainder must lower the 480s gate ceiling; leaving it fixed is what left \
             193 compat nodes unrun"
                .into(),
        );
    }
    let cold_compat = build_release_hermit_node("gate.manifest", "/tmp/target/release/hermit");
    if cold_compat.hint.preferred_inner_jobs != Some(8)
        || cold_compat.hint.classification != dagrun::model::StepClass::CpuBound
    {
        return Err("cold strict-compat release build lost its declared eight-job width".into());
    }
    let reused_compat = build_release_hermit_node(
        "gate.manifest",
        "/tmp/target/ci/hermit-strict",
    );
    if reused_compat.hint.preferred_inner_jobs.is_some()
        || reused_compat.hint.classification != dagrun::model::StepClass::Light
        || !reused_compat.cmd.starts_with("test -x ")
    {
        return Err("prebuilt strict-compat path stopped being a lightweight existence check".into());
    }
    if parse_git_depth(" 42\n")? != 42 {
        return Err("git-depth parser changed the measured value".into());
    }
    for bad in ["", "0", "-1", "not-a-depth", "1 2"] {
        if parse_git_depth(bad).is_ok() {
            return Err(format!("git-depth parser accepted invalid measurement {bad:?}"));
        }
    }
    let head = git_sha();
    let depth = measure_git_depth(&head)?;
    if depth == 0 {
        return Err("git-depth measurement accepted an impossible zero".into());
    }
    // The COMMAND-FAILURE branch, which the parser brackets above cannot reach.
    // `parse_git_depth` is only consulted when `git rev-list` exits zero, so a
    // parser that refuses every malformed string still says nothing about what
    // happens when the command itself fails -- and that is the case this field
    // exists for. Measured: `git rev-list --count 000...0` exits 128 with
    // "fatal: bad object", so this drives the `!output.status.success()` arm.
    // Without that arm the empty stdout would fall through to the parser and be
    // refused for the WRONG REASON, reporting a non-integer depth rather than a
    // failed command, so the assertion is on the message and not merely on
    // is_err().
    let absent = "0000000000000000000000000000000000000000";
    match measure_git_depth(absent) {
        Ok(depth) => {
            return Err(format!(
                "git-depth measurement invented {depth} for a commit that does not exist"
            ));
        }
        Err(error) => {
            if !error.contains("git rev-list --count") || !error.contains("failed with") {
                return Err(format!(
                    "git-depth refusal must name the failed command, not blame the parser: {error}"
                ));
            }
        }
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
    let exported = parse_argv(&[
        "portable-only".into(),
        "--write-constructed-dag".into(),
        "/tmp/constructed.json".into(),
    ])
    .map_err(|code| format!("constructed DAG export: valid argv refused with exit {code}"))?;
    if !exported.show_plan
        || exported.write_constructed_dag.as_deref() != Some(Path::new("/tmp/constructed.json"))
    {
        return Err("constructed DAG export: parser lost the output path or inert-plan mode".into());
    }
    let source = parse_argv(&[
        "full".into(),
        "--write-generated-plan".into(),
        "/tmp/source.json".into(),
    ])
    .map_err(|code| format!("source DAG export: valid argv refused with exit {code}"))?;
    if !source.show_plan
        || source.write_generated_plan.as_deref() != Some(Path::new("/tmp/source.json"))
    {
        return Err("source DAG export: parser lost the output path or inert-plan mode".into());
    }
    if parse_argv(&["--write-constructed-dag".into()]).is_ok()
        || parse_argv(&["--write-generated-plan".into()]).is_ok()
        || parse_argv(&[
            "--write-constructed-dag".into(),
            "/tmp/constructed.json".into(),
            "--show-plan-json".into(),
        ])
        .is_ok()
        || parse_argv(&[
            "--write-constructed-dag".into(),
            "/tmp/constructed.json".into(),
            "--write-generated-plan".into(),
            "/tmp/source.json".into(),
        ])
        .is_ok()
    {
        return Err("constructed DAG export: malformed or competing output forms were accepted".into());
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
    // Policy/data brackets are inert: none runs a gate, publishes a label,
    // writes the real ledger, or touches a PR. The one deliberate exception is
    // nested_scope_self_test: it uses a fresh disposable scope with strict
    // inner/run/scope/wrapper bounds, then proves a nested signal cannot stop it.
    for line in [
        safe_ci_scope::self_test()?,
        nested_scope_self_test()?,
        retry_timeout_bound_bracket(&root)?,
        scheduler_accounting_bracket()?,
        budget_reason_bracket()?,
        summary_listing_bracket()?,
        validate_super::self_test(&root)?,
        validate_envelope::self_test()?,
        validate_history::self_test()?,
        validate_receipt::self_test()?,
        validate_runtime::self_test()?,
        validate_classification::self_test()?,
        prebuilt_rust_script_plan_bracket(&root)?,
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
    pin_gate_receipt_bracket()?;
    scorecard_writeback_scope_bracket()?;
    host_capability_bracket(&root)?;
    coverage_schema_bracket()?;
    cell_results_schema_bracket()?;
    rebase_freshness_message_bracket()?;
    test_node_coverage_bracket()?;
    typed_libtest_count_bracket()?;
    ledger_gate_origin_bracket()?;
    requalification_plan_bracket(&root)?;
    tool_root_split_bracket()?;
    validate_series_writer_bracket()?;
    no_result_propagation_bracket()?;
    possible_missing_artifact_bracket()?;
    selective_subset_bracket(&root)?;
    only_plan_bracket(&root)?;
    self_output_bracket()?;
    checkout_attribution_bracket()?;
    product_front_door_bracket()?;
    product_front_door_process_bracket()?;
    // ---- DAG-config carry + ungrantable-resource brackets -------------------
    // BOTH directions. A check that refuses everything would pass the negative
    // case alone, so the positive case (a real lane admits) is load-bearing.
    {
        let root = repo_root();
        for lane in ["portable", "privileged"] {
            let mut base = validate_plan::lane_config(&root, lane)?;
            base.default_jobs_env =
                format!("VALIDATE_CARRY_{}_JOBS", lane.to_ascii_uppercase());
            // POSITIVE: a real lane's own config must carry, and must be grantable.
            let steps = base.steps.clone();
            let carried = validate_plan::config_from_base(&base, steps, "bracket");
            validate_plan::assert_config_carried(&base, &carried)
                .map_err(|e| format!("carry bracket: lane {lane} did not carry its config: {e}"))?;
            let mut missing_jobs_env = carried.clone();
            missing_jobs_env.default_jobs_env.clear();
            let jobs_env_error =
                validate_plan::assert_config_carried(&base, &missing_jobs_env)
                    .err()
                    .ok_or_else(|| {
                        format!("carry bracket: lane {lane} accepted a dropped default_jobs_env")
                    })?;
            if !jobs_env_error.contains("default_jobs_env") {
                return Err(format!(
                    "carry bracket: lane {lane} dropped jobs env but named {jobs_env_error}"
                ));
            }
            let bad = validate_plan::ungrantable_resources(&carried);
            if !bad.is_empty() {
                return Err(format!(
                    "grantable bracket: lane {lane} carried its caps yet still reports {} \
                     ungrantable demand(s): {:?}", bad.len(), &bad[..bad.len().min(3)]));
            }
            for unsupported in ["hermit_guest", "kvm"] {
                if base.resource_caps.contains_key(unsupported)
                    || base
                        .steps
                        .iter()
                        .any(|step| step.hint.resources.contains_key(unsupported))
                {
                    return Err(format!(
                        "resource-cap bracket: lane {lane} restored unsupported exclusive resource {unsupported}"
                    ));
                }
            }
            let mut cleared_cap_starvation = 0;
            let demanded_resources = base
                .steps
                .iter()
                .flat_map(|step| step.hint.resources.keys())
                .collect::<BTreeSet<_>>();
            if !demanded_resources.is_empty() {
                // NEGATIVE: drop declared caps exactly as the historical bug did
                // -> every remaining demand must be REFUSED and named rather
                // than sleeping forever. A selected label may retain global
                // caps that none of its own nodes demand; that is valid and has
                // no negative cap-removal case to construct.
                let mut stripped = carried.clone();
                stripped.resource_caps.clear();
                let starved = validate_plan::ungrantable_resources(&stripped);
                if starved.is_empty() {
                    return Err(format!(
                        "grantable bracket: lane {lane} with resource_caps CLEARED reported nothing \
                         ungrantable -- the check is inert and would not have caught the stall"));
                }
                let named = base
                    .resource_caps
                    .keys()
                    .any(|resource| starved.iter().any(|row| row.contains(resource)));
                if !named {
                    return Err(format!(
                        "grantable bracket: refusal for {lane} names no resource: {:?}",
                        &starved[..starved.len().min(2)]
                    ));
                }
                cleared_cap_starvation = starved.len();
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
                     base.resource_caps.len(), base.default_step_timeout, cleared_cap_starvation);
        }
    }
    // The full hot path selects the committed full-labelled graph and pays the
    // exact-tree manifest audit once. Bracket that immutable scheduler input
    // and the explicit refusal of the removed sequential-lanes spelling.
    {
        let root = repo_root();
        let tmp = std::env::temp_dir().join(format!("validate-plan-selftest-{}", std::process::id()));
        let full_args = parse_argv(&["full".into(), "--no-label-pr".into()])
            .map_err(|rc| format!("full-plan bracket: parser refused positive form rc={rc}"))?;
        let mut full = build_plan(&root, &full_args, &tmp)?;
        require_committed_scheduler_input(&full)?;
        let inherited_timeout_violations = steps_violating_run_timeout(&full.cfg, 1619);
        if !inherited_timeout_violations
            .iter()
            .any(|(tag, timeout)| tag == "check.lint_checks" && *timeout == 2400)
        {
            return Err(format!(
                "full-plan bracket: the inherited-timeout fixture no longer contains the raw \
                 check.lint_checks 2400s declaration: {inherited_timeout_violations:?}"
            ));
        }
        if steps_violating_run_timeout(&full.cfg, 1).is_empty() {
            return Err(
                "full-plan bracket: an impossible real execution timeout no longer refuses"
                    .into(),
            );
        }
        if full.second.is_some() {
            return Err("full-plan bracket: default full plan is still sequential".into());
        }
        let manifest_nodes: Vec<String> = full
            .cfg
            .steps
            .iter()
            .filter(|s| validation_step_identity(s) == ValidationStepIdentity::ManifestAudit)
            .map(|s| s.tag())
            .collect();
        if manifest_nodes != vec!["gate.manifest"] {
            return Err(format!(
                "full-plan bracket: exact-tree manifest audit was not exactly gate.manifest: {manifest_nodes:?}"
            ));
        }
        // The one committed audit must run AFTER the node that builds the binary it
        // invokes, and that builder must not wait on the audit. Losing this edge
        // in the former runtime deduplication made every cold full run die at
        // `exit 127: target/debug/test-harness: No such file or directory` with
        // 56 of 59 nodes skipped, and made every warm run audit the tree with a
        // stale binary. Asserted on the real full plan, not a fixture.
        let builder = "setup.manifest_plan";
        let find_deps = |tag: &str| {
            full.cfg
                .steps
                .iter()
                .find(|s| s.tag() == tag)
                .map(|s| s.deps.clone())
        };
        let audit_deps = find_deps("gate.manifest")
            .ok_or_else(|| "full-plan bracket: gate.manifest disappeared".to_string())?;
        let builder_deps = find_deps(builder)
            .ok_or_else(|| format!("full-plan bracket: {builder} disappeared"))?;
        if !audit_deps.iter().any(|d| d == builder) {
            return Err(format!(
                "full-plan bracket: gate.manifest does not depend on {builder}, so a cold run cannot build the binary it invokes: deps={audit_deps:?}"
            ));
        }
        if builder_deps.iter().any(|d| d == "gate.manifest") {
            return Err(format!(
                "full-plan bracket: {builder} still waits on gate.manifest, which is the cycle the dependency union must break: deps={builder_deps:?}"
            ));
        }
        let manifest_audit = full
            .cfg
            .steps
            .iter()
            .find(|step| validation_step_identity(step) == ValidationStepIdentity::ManifestAudit)
            .expect("manifest audit exists")
            .clone();
        let manifest_producer = full
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
            .ok_or("full-plan bracket: manifest-plan producer disappeared")?;
        if manifest_producer.cmd
            != format!(
                "{RUST_SCRIPT_COMMAND_PREFIX}{}",
                validate_plan::MANIFEST_PLAN_BUILD_COMMAND
            )
            || manifest_producer.deps != [RUST_SCRIPT_PRODUCER_TAG.to_string()]
            || manifest_producer.deps.iter().any(|dependency| dependency == "gate.manifest")
        {
            return Err(format!(
                "full-plan bracket: committed manifest-plan producer is not directly after the rust-script producer: cmd={} deps={:?}",
                manifest_producer.cmd, manifest_producer.deps
            ));
        }
        if manifest_audit.cmd
            != format!("{RUST_SCRIPT_COMMAND_PREFIX}{MANIFEST_AUDIT_COMMAND}")
        {
            return Err(format!(
                "full-plan bracket: manifest audit has unexpected invocation: {}",
                manifest_audit.cmd
            ));
        }
        if manifest_audit.deps
            != [validate_plan::MANIFEST_PLAN_PRODUCER_TAG.to_string()]
        {
            return Err(format!(
                "full-plan bracket: manifest audit can run without its binary producer: deps={:?}",
                manifest_audit.deps
            ));
        }
        println!("  {}", manifest_producer_edge_bracket(&full.cfg)?);
        let pin_nodes: Vec<String> = full
            .cfg
            .steps
            .iter()
            .filter(|s| s.cmd.contains("ci/run-reverie-pin-check.sh"))
            .map(|s| s.tag())
            .collect();
        if pin_nodes != vec![PIN_GATE_TAG] {
            return Err(format!(
                "full-plan bracket: committed graph has more than one pin authority: {pin_nodes:?}"
            ));
        }
        for required in ["compat.echo", "privileged-cpuid.faulting"] {
            if !full.cfg.steps.iter().any(|s| s.tag() == required) {
                return Err(format!("full-plan bracket: committed plan lost {required}"));
            }
        }
        if full.cfg.steps.iter().any(|step| {
            step.cmd.contains("scripts/validate.rs --portable-strict-compat-only")
                || step.cmd.contains("pressure-test.rs")
                || step.cmd.contains("dagrun run")
                || step.cmd.contains("run_dag_boxed")
        }) {
            return Err(
                "full-plan bracket: a direct strict-compat node still starts another scheduler"
                    .into(),
            );
        }
        if full.compat != Some(CompatMode::PortableStrict) {
            return Err(
                "full-plan bracket: flattened portable compatibility lost its typed verdict"
                    .into(),
            );
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
        let integration = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "test.hermit_integration")
            .ok_or("full-plan bracket: Hermit integration node disappeared")?;
        if !hermit_integration_uses_published_artifact(integration) {
            return Err(format!(
                "full-plan bracket: Hermit integration tests can consume a mutable Hermit binary: cmd={} deps={:?}",
                integration.cmd, integration.deps
            ));
        }
        let mut missing_wrapper = integration.clone();
        let after_rust_script_prefix = missing_wrapper
            .cmd
            .strip_prefix(RUST_SCRIPT_COMMAND_PREFIX)
            .ok_or("full-plan bracket: integration node lost its rust-script prefix")?;
        let without_wrapper = after_rust_script_prefix
            .strip_prefix(INTEGRATION_ARTIFACT_WRAPPER)
            .ok_or("full-plan bracket: cannot plant missing integration artifact wrapper")?;
        missing_wrapper.cmd = format!("{RUST_SCRIPT_COMMAND_PREFIX}{without_wrapper}");
        if hermit_integration_uses_published_artifact(&missing_wrapper) {
            return Err(
                "full-plan bracket: removing the integration artifact wrapper was accepted"
                    .into(),
            );
        }
        let mut missing_dependency = integration.clone();
        missing_dependency
            .deps
            .retain(|dependency| dependency != "build.e2e_artifact");
        if hermit_integration_uses_published_artifact(&missing_dependency) {
            return Err(
                "full-plan bracket: removing the integration artifact dependency was accepted"
                    .into(),
            );
        }
        let manifest_consumers: Vec<_> = full
            .cfg
            .steps
            .iter()
            .filter(|s| validation_step_identity(s) == ValidationStepIdentity::ManifestRun)
            .collect();
        if manifest_consumers.is_empty() {
            return Err("full-plan bracket: no manifest consumers were inspected".into());
        }
        let manifest_tags = manifest_consumers
            .iter()
            .map(|step| step.tag())
            .collect::<BTreeSet<_>>();
        let scorecard_deps = full
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == "full-scorecard.compatibility")
            .ok_or("full-plan bracket: compatibility scorecard disappeared")?
            .deps
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_scorecard_deps = manifest_tags
            .iter()
            .cloned()
            .chain(["gate.manifest".to_string(), PIN_GATE_TAG.to_string()])
            .collect::<BTreeSet<_>>();
        if scorecard_deps != expected_scorecard_deps {
            return Err(format!(
                "full-plan bracket: compatibility scorecard must depend on every manifest result node and both focused preflight gates: expected={expected_scorecard_deps:?}, actual={scorecard_deps:?}"
            ));
        }
        let mut results_paths = BTreeSet::new();
        let mut junit_paths = BTreeSet::new();
        let mut spelling_probe = (*manifest_consumers[0]).clone();
        spelling_probe.cmd = "changed invocation text".into();
        if validation_step_identity(&spelling_probe) != ValidationStepIdentity::ManifestRun {
            return Err(
                "full-plan bracket: manifest-run identity still depends on command text".into(),
            );
        }
        for consumer in manifest_consumers {
            let DagManifest { lane, .. } = consumer.manifest.as_ref().ok_or_else(|| {
                format!(
                    "full-plan bracket: {} manifest consumer lacks typed manifest selection",
                    consumer.tag()
                )
            })?;
            let result_path = format!(
                "\"$E2E_RESULT_ROOT/{lane}/{}/results.jsonl\"",
                consumer.job
            );
            let junit_path = format!(
                "\"$E2E_RESULT_ROOT/{lane}/{}/junit.xml\"",
                consumer.job
            );
            if consumer.cmd.matches("--results").count() != 1
                || !consumer.cmd.contains(&format!("--results {result_path}"))
                || !results_paths.insert(result_path)
            {
                return Err(format!(
                    "full-plan bracket: {} does not have one unique result path: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            if consumer.env.contains_key("E2E_ATTEMPT") {
                return Err(format!(
                    "full-plan bracket: {} still receives the outer E2E_ATTEMPT variable: {:?}",
                    consumer.tag(), consumer.env
                ));
            }
            if consumer.cmd.matches("--junit").count() != 1
                || !consumer.cmd.contains(&format!("--junit {junit_path}"))
                || !junit_paths.insert(junit_path)
            {
                return Err(format!(
                    "full-plan bracket: {} does not have one unique JUnit path: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            if !consumer
                .cmd
                .starts_with("./ci/hermetic/run-in-pinned-root.sh ")
                || !consumer.cmd.contains("run-with-hermit-e2e-artifact.sh")
            {
                return Err(format!(
                    "full-plan bracket: {} is not a committed pinned-root consumer of the published Hermit artifact: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            let producer = if lane == "portable" {
                if !consumer.cmd.contains("--require-install") {
                    return Err(format!(
                        "full-plan bracket: portable consumer {} did not require the backend-resource bundle",
                        consumer.tag()
                    ));
                }
                "build.e2e_artifact_in_pinned_root"
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
        for required in ["build.e2e_artifact", "build.liteinst_runtime_release"] {
            if !privileged_build.deps.iter().any(|dependency| dependency == required) {
                return Err(format!(
                    "full-plan bracket: privileged build can start before required build barrier {required}"
                ));
            }
        }
        if !privileged_build.cmd.contains("verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path")
            || !privileged_build.cmd.contains(NEXTEST_PRIVILEGED_ASSERT_COMMAND)
            || !privileged_build.cmd.contains(TESTS_MISC_EXECUTABLE_READ_COMMAND)
            || privileged_build.cmd.contains("cargo ")
            || !portable_build.cmd.ends_with(NEXTEST_PORTABLE_PREPARE_COMMAND)
        {
            return Err("full-plan bracket: prepared Nextest population must come from the workspace producer and the privileged barrier must verify it without Cargo compilation".into());
        }
        let prepared = hermit_manifest_plan::nextest_binaries::profile_selections(&root, "portable")?;
        for required in hermit_manifest_plan::nextest_binaries::profile_selections(&root, "privileged")?.keys() {
            if !prepared.contains_key(required) {
                return Err(format!("full-plan bracket: portable preparation omits privileged Cargo selection {required}"));
            }
        }
        if ["test.cli", "test.hermit_modes"]
            .iter()
            .any(|forbidden| privileged_build.deps.iter().any(|dep| dep == forbidden))
        {
            return Err(format!(
                "full-plan bracket: privileged build depends on portable test success: {:?}",
                privileged_build.deps
            ));
        }
        assert_committed_shared_integration_test_serialization(
            &full.cfg.steps,
            &full.cfg.resource_caps,
        )?;
        let mut missing_shared_demand = full.cfg.steps.clone();
        missing_shared_demand
            .iter_mut()
            .find(|step| step.tag() == "test.cli")
            .expect("portable cli exists")
            .hint
            .resources
            .remove("integration_test_binaries.cli");
        let mut missing_shared_cap = full.cfg.resource_caps.clone();
        missing_shared_cap.remove("integration_test_binaries.hermit_modes");
        if assert_committed_shared_integration_test_serialization(
            &missing_shared_demand,
            &full.cfg.resource_caps,
        )
        .is_ok()
            || assert_committed_shared_integration_test_serialization(&full.cfg.steps, &missing_shared_cap)
                .is_ok()
        {
            return Err("full-plan bracket: missing shared-test resource demand/cap was accepted".into());
        }
        let committed = validate_plan::validation_config(&root)?;
        let local_privileged =
            dagrun::select_steps_by_labels(&committed, &["privileged".into()])?;
        let selected = dagrun::select_steps_by_tags(
            &local_privileged,
            &[
                "privileged-only-test.cli_kvm".into(),
                "privileged-only-test.pmu_buck_chaos_cases".into(),
            ],
            false,
        )?;
        let tags: BTreeSet<String> = selected.steps.iter().map(Step::tag).collect();
        if ["test.cli", "test.hermit_modes"].iter().any(|tag| tags.contains(*tag)) {
            return Err(format!(
                "full-plan bracket: selected privileged tests acquired a portable-test dependency: {tags:?}"
            ));
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
        if !cpuid
            .deps
            .iter()
            .any(|dependency| dependency == "privileged-build.privileged_tests")
        {
            return Err(
                "full-plan bracket: CPUID consumer can run before tests_misc is built".into(),
            );
        }
        let deps_of = |tag: &str| {
            full
                .cfg
                .steps
                .iter()
                .find(|step| step.tag() == tag)
                .map(|step| step.deps.clone())
        };
        for tag in [
            "build.manifest_guests_in_pinned_root",
            "privileged-build.manifest_guests_in_pinned_root",
        ] {
            let deps = deps_of(tag).ok_or_else(|| {
                format!("full-plan bracket: post-fusion pinned-root producer {tag} disappeared")
            })?;
            if !deps
                .iter()
                .any(|dependency| dependency == "setup.manifest_plan_in_pinned_root")
            {
                return Err(format!(
                    "full-plan bracket: post-fusion {tag} can run before \
                     setup.manifest_plan_in_pinned_root builds the test harness: deps={deps:?}"
                ));
            }
        }
        for step in full.cfg.steps.iter().filter(|step| {
            step.tag().ends_with("_in_pinned_root")
                || validation_step_identity(step) == ValidationStepIdentity::ManifestRun
        }) {
            if !step
                .cmd
                .contains("/src/ci/hermetic/assert-build-dependencies.sh")
            {
                return Err(format!(
                    "full-plan bracket: pinned-root node {} can start without the build-dependency assertion: {}",
                    step.tag(), step.cmd
                ));
            }
        }
        for tag in [
            "privileged-e2e.manifest_applications",
            "privileged-e2e.manifest_backend_parity_c",
        ] {
            let deps = deps_of(tag)
                .ok_or_else(|| format!("full-plan bracket: pinned-root cell {tag} disappeared"))?;
            for required in [
                "build.e2e_artifact_in_pinned_root",
                "privileged-build.manifest_guests_in_pinned_root",
            ] {
                if !deps.iter().any(|dependency| dependency == required) {
                    return Err(format!(
                        "full-plan bracket: {tag} does not wait for pinned-root producer \
                         {required}: deps={deps:?}"
                    ));
                }
            }
        }
        let original_command = full.cfg.steps[0].cmd.clone();
        full.cfg.steps[0].cmd.push_str(" --planted-runtime-remix");
        if require_committed_scheduler_input(&full).is_ok() {
            return Err(
                "full-plan bracket: a planted post-selection command rewrite reached the scheduler boundary"
                    .into(),
            );
        }
        full.cfg.steps[0].cmd = original_command;
        require_committed_scheduler_input(&full)?;
        let sequential = parse_argv(&[
            "full".into(),
            "--sequential-lanes".into(),
            "--no-label-pr".into(),
        ]);
        if !matches!(sequential, Err(2)) {
            return Err(
                "full-plan bracket: removed --sequential-lanes was not explicitly refused".into(),
            );
        }
        println!(
            "  full plan: {} committed labelled node(s), 1 manifest-plan producer -> 1 exact-tree manifest audit + 1 pin authority; removed sequential-lanes spelling refused",
            full.cfg.steps.len()
        );
    }

    // This bracket clones the committed tree and therefore must exercise the
    // recorded submodule API rather than any locally materialized dependency.
    // Keep it last so all Hermit-only policy brackets report independently.
    println!(
        "  {}",
        submodule_failure_service_result_bracket(&repo_root())?
    );

    Ok(())
}

/// Assert that the public option names only the two inner checks it skips.
///
/// The parent `ci-hub validate-lock` admission runs before this option reaches
/// `scripts/validate.rs`. The option therefore must not imply that it admits a
/// dirty or stale validation target through that earlier check.
fn inner_freshness_skip_cli_bracket() -> Result<(), String> {
    if parse_argv(&["--run-on-dirty-tree".into(), "--self-test".into()]).is_ok() {
        return Err("inner freshness skip: the misleading old option is still accepted".into());
    }
    let parsed = parse_argv(&[
        SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION.into(),
        "--self-test".into(),
    ])
    .map_err(|code| {
        format!(
            "inner freshness skip: parser refused {} with exit {code}",
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION
        )
    })?;
    if !parsed.skip_inner_dirty_working_tree_and_rebase_freshness_checks {
        return Err(format!(
            "inner freshness skip: {} did not select the two inner checks",
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION
        ));
    }

    let help = usage();
    for required in [
        SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
        SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_ENV,
        "Skip only scripts/validate.rs's dirty-working-tree and",
        "rebase-freshness checks; does not bypass ci-hub validate-lock",
        "admission. AGENTS SHOULD NOT USE THIS.",
    ] {
        if !help.contains(required) {
            return Err(format!(
                "inner freshness skip: help omitted required text {required:?}"
            ));
        }
    }
    for removed in ["--run-on-dirty-tree", "VALIDATE_RUN_ON_DIRTY_TREE"] {
        if help.contains(removed) {
            return Err(format!(
                "inner freshness skip: help still advertises misleading name {removed}"
            ));
        }
    }
    println!(
        "  inner freshness skip: new option accepted, old option refused, and help names only the two inner checks"
    );
    Ok(())
}

/// Execute the production manifest producer/audit dependency spine in both
/// directions. The positive case starts with no output and requires the
/// producer to create it before the audit runs. The negative case makes the
/// producer fail and requires the scheduler to dependency-skip the audit.
fn manifest_producer_edge_bracket(cfg: &DagConfig) -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!(
        "validate-manifest-producer-edge-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    std::fs::create_dir(&tmp)
        .map_err(|error| format!("manifest producer bracket: cannot create {}: {error}", tmp.display()))?;

    let result = (|| -> Result<(), String> {
        let required = [
            "pre.submodules",
            PIN_GATE_TAG,
            validate_plan::MANIFEST_PLAN_PRODUCER_TAG,
            "gate.manifest",
        ];
        let fixture = |producer_cmd: String, gate_cmd: String| -> Result<DagConfig, String> {
            let mut steps = Vec::new();
            for tag in required {
                let source = cfg
                    .steps
                    .iter()
                    .find(|step| step.tag() == tag)
                    .ok_or_else(|| format!("manifest producer bracket: production plan lost {tag}"))?;
                let mut step = step_with_caps(
                    &source.group,
                    &source.job,
                    "manifest producer dependency fixture",
                    match tag {
                        "pre.submodules" | PIN_GATE_TAG => "true".to_string(),
                        validate_plan::MANIFEST_PLAN_PRODUCER_TAG => producer_cmd.clone(),
                        "gate.manifest" => gate_cmd.clone(),
                        _ => unreachable!(),
                    },
                    source.deps.clone(),
                    30,
                    30,
                    64 * 1024 * 1024,
                );
                step.deps.retain(|dependency| required.contains(&dependency.as_str()));
                steps.push(step);
            }
            Ok(validate_plan::config_from(
                steps,
                "manifest producer dependency fixture",
            ))
        };

        let output = tmp.join("target/debug/test-harness");
        let gate_ran = tmp.join("gate-ran");
        let output_parent = output
            .parent()
            .ok_or_else(|| format!("manifest producer bracket: {} has no parent", output.display()))?;
        let positive = fixture(
            format!(
                "mkdir -p {parent} && printf '#!/bin/sh\\nexit 0\\n' > {output} && chmod +x {output}",
                parent = validate_plan::shell_quote(&output_parent.to_string_lossy()),
                output = validate_plan::shell_quote(&output.to_string_lossy()),
            ),
            format!(
                "test -x {output} && {output} && : > {gate_ran}",
                output = validate_plan::shell_quote(&output.to_string_lossy()),
                gate_ran = validate_plan::shell_quote(&gate_ran.to_string_lossy()),
            ),
        )?;
        let positive_result = run_lane_once(
            &positive,
            2,
            true,
            0,
            None,
            &tmp.join("positive.log"),
            None,
            false,
        );
        if !positive_result.complete
            || !positive_result.ok
            || positive_result.outcomes.len() != required.len()
            || !positive_result.skipped.is_empty()
            || !output.is_file()
            || !gate_ran.is_file()
        {
            return Err(format!(
                "manifest producer bracket: absent output was not produced before the gate: complete={} ok={} outcomes={:?} skipped={:?} output={} gate_ran={}",
                positive_result.complete,
                positive_result.ok,
                positive_result
                    .outcomes
                    .iter()
                    .map(|outcome| (outcome.tag.as_str(), outcome.ok))
                    .collect::<Vec<_>>(),
                positive_result.skipped,
                output.is_file(),
                gate_ran.is_file()
            ));
        }

        std::fs::remove_file(&output)
            .map_err(|error| format!("manifest producer bracket: cannot remove {}: {error}", output.display()))?;
        std::fs::remove_file(&gate_ran)
            .map_err(|error| format!("manifest producer bracket: cannot remove {}: {error}", gate_ran.display()))?;
        let negative = fixture(
            "exit 23".to_string(),
            format!(
                ": > {}",
                validate_plan::shell_quote(&gate_ran.to_string_lossy())
            ),
        )?;
        let negative_result = run_lane_once(
            &negative,
            2,
            true,
            0,
            None,
            &tmp.join("negative.log"),
            None,
            false,
        );
        let producer_failed = negative_result.outcomes.iter().any(|outcome| {
            outcome.tag == validate_plan::MANIFEST_PLAN_PRODUCER_TAG
                && !outcome.ok
                && outcome.returncode == Some(23)
        });
        if negative_result.complete
            || negative_result.ok
            || !producer_failed
            || negative_result.skipped != ["gate.manifest".to_string()]
            || gate_ran.exists()
        {
            return Err(format!(
                "manifest producer bracket: failed producer did not block the gate: complete={} ok={} producer_failed={producer_failed} outcomes={:?} skipped={:?} gate_ran={}",
                negative_result.complete,
                negative_result.ok,
                negative_result
                    .outcomes
                    .iter()
                    .map(|outcome| (outcome.tag.as_str(), outcome.ok, outcome.returncode))
                    .collect::<Vec<_>>(),
                negative_result.skipped,
                gate_ran.exists()
            ));
        }
        Ok(())
    })();

    let cleanup = std::fs::remove_dir_all(&tmp)
        .map_err(|error| format!("manifest producer bracket: cannot remove {}: {error}", tmp.display()));
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(
            "manifest producer edge: absent output built before gate; producer failure dependency-skips gate"
                .into(),
        ),
        (Err(problem), Ok(())) => Err(problem),
        (Ok(()), Err(cleanup_problem)) => Err(cleanup_problem),
        (Err(problem), Err(cleanup_problem)) => Err(format!(
            "{problem}; cleanup also failed: {cleanup_problem}"
        )),
    }
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

fn ledger_schema_version(
    coverage_schema: i64,
    cell_results: Option<&validate_cell_results::RetainedCellResults>,
) -> i64 {
    cell_results
        .map(|results| results.schema_version)
        .unwrap_or(coverage_schema)
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

/// Bind the outer ledger version to the retained payload that defines its
/// shape. This makes reverting the payload version without changing the writer
/// fail in `--self-test`, rather than silently relabeling new evidence as an old
/// schema.
fn cell_results_schema_bracket() -> Result<(), String> {
    let retained = validate_cell_results::RetainedCellResults {
        schema_version: validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION,
        run_id: "schema-bracket".into(),
        evidence: serde_json::json!({}),
    };
    let current = ledger_schema_version(COVERAGE_LEDGER_SCHEMA_VERSION, Some(&retained));
    if current != 7 {
        return Err(format!(
            "cell-results schema: current payload must emit schema 7, got {current}"
        ));
    }
    if ledger_schema_version(COVERAGE_LEDGER_SCHEMA_VERSION, None)
        != COVERAGE_LEDGER_SCHEMA_VERSION
    {
        return Err("cell-results schema: a row without cell results changed schema".into());
    }
    println!("  cell-results schema: current payload -> schema 7; absent payload -> schema 5");
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
        (" M ci/dag/validate.json", "a DAG change under ci/, but not the ledger"),
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

/// Exercise source attribution with real git state in both checkout modes.
///
/// Run 1573 left `.hermit-verify-summary-*` files when killed Hermit children
/// could not drop their private `NamedTempFile`s. The positive control creates
/// that exact path shape after a clean disposable checkout was admitted. The
/// negative control dirties an in-place source checkout and exercises the same
/// refusal predicate used by the front door. No pathname is excused.
fn checkout_attribution_bracket() -> Result<(), String> {
    let temporary = tempfile::tempdir()
        .map_err(|error| format!("checkout attribution: cannot create fixture root: {error}"))?;
    let parent = temporary.path().join("dev-hermit");
    let source = parent.join("hermit");
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let host = "fixture-host";
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("checkout attribution: cannot read boot id: {error}"))?;
    let boot_id = boot_id.trim();
    let owner_pid = std::process::id() as i32;
    let (_, owner_start_ticks) = validate_runtime::process_identity(owner_pid)
        .ok_or("checkout attribution: cannot read this process identity")?;
    std::fs::create_dir_all(&source)
        .map_err(|error| format!("checkout attribution: cannot create fixture: {error}"))?;
    let git = |repo: &Path, args: &[&str]| -> Result<(), String> {
        let status = Command::new("git")
            .current_dir(repo)
            .args(args)
            .status()
            .map_err(|error| format!("checkout attribution: cannot run git {args:?}: {error}"))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("checkout attribution: git {args:?} exited {status}"))
    };
    git(&source, &["init", "-q"])?;
    std::fs::write(source.join("tracked.txt"), b"clean source\n")
        .map_err(|error| format!("checkout attribution: cannot write fixture: {error}"))?;
    git(&source, &["add", "tracked.txt"])?;
    git(
        &source,
        &[
            "-c",
            "user.email=validate-fixture@example.invalid",
            "-c",
            "user.name=validate fixture",
            "commit",
            "-qm",
            "fixture",
        ],
    )?;

    // Negative direction: ordinary source dirt still hits the real front-door
    // refusal predicate and cannot become receipt-eligible.
    std::fs::write(source.join("tracked.txt"), b"dirty source\n")
        .map_err(|error| format!("cannot dirty source fixture: {error}"))?;
    let dirty_source = worktree_dirty_at(&source);
    if !dirty_source || !dirty_worktree_requires_refusal(false, dirty_source, false) {
        return Err("checkout attribution: a genuinely dirty in-place source did not refuse".into());
    }
    if is_disposable_validate_checkout(&source, Some(&parent)) {
        return Err("checkout attribution: the in-place source was classified as disposable".into());
    }
    let in_place_attribution = source_attribution(commit, false, true, false);
    if in_place_attribution.commit_anchored || !in_place_attribution.tree_dirty {
        return Err("checkout attribution: a dirty in-place checkout remained anchored".into());
    }
    if validate_receipt::eligible(
        0,
        0,
        true,
        in_place_attribution.commit_anchored,
        in_place_attribution.tree_dirty,
        "full",
    )
    .is_ok()
    {
        return Err("checkout attribution: a dirty in-place result became receipt-eligible".into());
    }

    // Positive direction: materialize the same clean commit at ci-hub's exact
    // disposable boundary, then plant the path shape that dirtied run 1573.
    git(&source, &["restore", "tracked.txt"])?;
    let disposable = parent.join("worktrees/validate/validate-fresh-fixture");
    std::fs::create_dir_all(disposable.parent().unwrap())
        .map_err(|error| format!("checkout attribution: cannot create validate root: {error}"))?;
    std::fs::rename(&source, &disposable)
        .map_err(|error| format!("checkout attribution: cannot materialize fixture: {error}"))?;
    if tree_dirty_at(&disposable)
        || !is_disposable_validate_checkout(&disposable, Some(&parent))
    {
        return Err("checkout attribution: disposable fixture was not clean and recognized".into());
    }
    let authority = serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "admissible": true,
        "state": "held",
        "reason_code": null,
        "canonical_anchor_held": true,
        "cleanup_state": "active-bound",
        "holder": {"kind": "validate", "target": commit, "host": host},
        "owner": {
            "host": host,
            "liveness": "alive",
            "pid": owner_pid,
            "start_ticks": owner_start_ticks,
            "boot_id": boot_id
        }
    }))
    .map_err(|error| format!("checkout attribution: cannot encode authority: {error}"))?;
    let prior_stop_mode = std::env::var_os("HERMIT_VALIDATE_STOP_TEST_MODE");
    let prior_authority = std::env::var_os("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON");
    unsafe {
        std::env::set_var("HERMIT_VALIDATE_STOP_TEST_MODE", "1");
        std::env::set_var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON", &authority);
    }
    let admitted = checkout_admission(&disposable, Some(&parent), None, commit, host);
    let lookalike = parent.join("worktrees/slots/validate-fresh-lookalike");
    let lookalike_admitted = checkout_admission(&lookalike, Some(&parent), None, commit, host);
    unsafe { std::env::set_var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON", "{}") };
    let missing_authority = checkout_admission(&disposable, Some(&parent), None, commit, host);
    unsafe {
        match prior_stop_mode {
            Some(value) => std::env::set_var("HERMIT_VALIDATE_STOP_TEST_MODE", value),
            None => std::env::remove_var("HERMIT_VALIDATE_STOP_TEST_MODE"),
        }
        match prior_authority {
            Some(value) => std::env::set_var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON", value),
            None => std::env::remove_var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON"),
        }
    }
    if !admitted.lock_admitted || !admitted.disposable {
        return Err("checkout attribution: canonical disposable + live lock was not admitted".into());
    }
    std::fs::write(disposable.join(".hermit-verify-summary-fixture"), b"")
        .map_err(|error| format!("cannot plant Hermit summary fixture: {error}"))?;
    let disposable_attribution = source_attribution(
        commit,
        false,
        tree_dirty_at(&disposable),
        admitted.disposable,
    );
    if disposable_attribution
        != (SourceAttribution {
            commit_anchored: true,
            tree_dirty: false,
        })
        || validate_receipt::eligible(
            0,
            0,
            true,
            disposable_attribution.commit_anchored,
            disposable_attribution.tree_dirty,
            "full",
        )
        .is_err()
    {
        return Err(
            "checkout attribution: a clean admitted disposable checkout lost commit anchoring after its own tracked-path write"
                .into(),
        );
    }
    if source_attribution(commit, true, false, admitted.disposable).commit_anchored {
        return Err("checkout attribution: dirt present at admission was ignored".into());
    }
    if missing_authority.lock_admitted
        || missing_authority.disposable
        || source_attribution(commit, false, true, missing_authority.disposable).commit_anchored
    {
        return Err("checkout attribution: a missing canonical lock authorized disposal".into());
    }
    if !lookalike_admitted.lock_admitted
        || lookalike_admitted.disposable
        || source_attribution(commit, false, true, lookalike_admitted.disposable).commit_anchored
    {
        return Err("checkout attribution: an ordinary-slot lookalike was treated as disposable".into());
    }

    println!(
        "  checkout attribution: clean disposable + Hermit summary remains anchored; dirty in-place source refuses"
    );
    Ok(())
}

/// Bracket the `--selective` subset builder against the REAL portable lane.
///
/// The dangerous failure here is silent under-running: a subset that drops a
/// node the selector asked for, or keeps a dangling dependency that makes the
/// runner skip a selected node. Both are checked against the portable label in
/// `ci/dag/validate.json`
/// itself rather than a fixture, because a fixture would not notice the lane
/// file changing shape underneath the selector.
fn selective_subset_bracket(root: &Path) -> Result<(), String> {
    let source_path = validate_plan::validation_dag_path(root);
    let source_before = std::fs::read(&source_path)
        .map_err(|error| format!("selective bracket: cannot read source DAG: {error}"))?;
    let committed = validate_plan::validation_config(root)?;
    let portable = dagrun::select_steps_by_labels(&committed, &["portable".into()])?;
    let all_tags: BTreeSet<String> = portable.steps.iter().map(Step::tag).collect();
    let (child, parent) = portable
        .steps
        .iter()
        .find_map(|step| {
            step.deps
                .iter()
                .find(|dependency| all_tags.contains(*dependency))
                .map(|dependency| (step.tag(), dependency.clone()))
        })
        .ok_or("selective bracket: portable label has no dependency edge")?;

    let requested = [child.clone()].into_iter().collect::<BTreeSet<_>>();
    let selected = select_from_committed_decision(
        &portable,
        portable.steps.len(),
        SelectDecision::Nodes(requested),
    )?;
    let expected =
        dagrun::select_steps_by_tags(&portable, std::slice::from_ref(&child), false)?;
    if dag_to_json(&selected) != dag_to_json(&expected) {
        return Err(
            "selective bracket: since-green selection differs from dagrun's typed ID selection"
                .into(),
        );
    }
    let selected_tags = selected.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    if !selected_tags.contains(&child) || !selected_tags.contains(&parent) {
        return Err(format!(
            "selective bracket: dependency-closed selection lost child={child} or parent={parent}: {selected_tags:?}"
        ));
    }

    let mapped_e2e = local_validation_step_tag("e2e.manifest_applications_on_host");
    if mapped_e2e != "e2e.manifest_applications"
        || local_validation_step_tag("test.detcore_unit") != "test.detcore_unit"
    {
        return Err("selective bracket: hosted-to-local step identity mapping drifted".into());
    }
    let mapped_plan = select_from_committed_decision(
        &portable,
        portable.steps.len(),
        SelectDecision::Nodes([mapped_e2e.clone()].into_iter().collect()),
    )?;
    let mapped_tags = mapped_plan.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    if !mapped_tags.contains(&mapped_e2e)
        || mapped_tags.contains("e2e.manifest_applications_on_host")
    {
        return Err(format!(
            "selective bracket: hosted result identity did not select its local committed counterpart: {mapped_tags:?}"
        ));
    }

    let unknown = ["no.such_node".to_string()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let error = select_from_committed_decision(
        &portable,
        portable.steps.len(),
        SelectDecision::Nodes(unknown),
    )
    .err()
    .ok_or("selective bracket: an unknown selected ID was accepted")?;
    if !error.contains("unknown step tag") {
        return Err(format!(
            "selective bracket: unknown-ID refusal lost its diagnosis: {error}"
        ));
    }

    let skip = select_from_committed_decision(
        &portable,
        portable.steps.len(),
        SelectDecision::Skip,
    )?;
    let expected_skip = [
        "pre.submodules",
        PIN_GATE_TAG,
        RUST_SCRIPT_PRODUCER_TAG,
        validate_plan::MANIFEST_PLAN_PRODUCER_TAG,
        "gate.manifest",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<BTreeSet<_>>();
    let actual_skip = skip.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    if actual_skip != expected_skip {
        return Err(format!(
            "selective bracket: no-change selection did not retain exactly committed preflight: expected={expected_skip:?} actual={actual_skip:?}"
        ));
    }

    let source_after = std::fs::read(&source_path)
        .map_err(|error| format!("selective bracket: cannot re-read source DAG: {error}"))?;
    if source_after != source_before {
        return Err("selective bracket: selecting IDs changed ci/dag/validate.json".into());
    }
    println!(
        "  selective subset: typed ID selection retained {child} and dependency {parent}; unknown IDs refused; no-change retained exact preflight; source bytes unchanged"
    );
    Ok(())
}

fn only_plan_bracket(root: &Path) -> Result<(), String> {
    let source_path = validate_plan::validation_dag_path(root);
    let source_before = std::fs::read(&source_path)
        .map_err(|error| format!("only bracket: cannot read source DAG: {error}"))?;
    let committed = validate_plan::validation_config(root)?;
    let portable = dagrun::select_steps_by_labels(&committed, &["portable".into()])?;
    let all_tags = portable.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    let (child, parent) = portable
        .steps
        .iter()
        .find_map(|step| {
            step.deps
                .iter()
                .find(|dependency| all_tags.contains(*dependency))
                .map(|dependency| (step.tag(), dependency.clone()))
        })
        .ok_or("only bracket: portable label has no dependency edge")?;

    let args = parse_argv(&[
        "--only".into(),
        "portable".into(),
        format!("{parent},{child}"),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: CLI refused a valid selection with exit {code}"))?;
    let mut plan = build_plan(root, &args, &std::env::temp_dir())?;
    let mut expected_tags = [child.clone(), parent.clone()]
        .into_iter()
        .collect::<BTreeSet<_>>();
    expected_tags.extend(
        [
            "pre.submodules",
            PIN_GATE_TAG,
            RUST_SCRIPT_PRODUCER_TAG,
            validate_plan::MANIFEST_PLAN_PRODUCER_TAG,
            "gate.manifest",
        ]
        .into_iter()
        .map(str::to_string),
    );
    let expected = dagrun::select_steps_by_tags(
        &portable,
        &expected_tags.iter().cloned().collect::<Vec<_>>(),
        true,
    )?;
    if dag_to_json(&plan.cfg) != dag_to_json(&expected)
        || plan.selection_mode != "only"
        || plan.suite_complete
        || plan.second.is_some()
    {
        return Err(
            "only bracket: runtime selection differs from exact committed IDs plus preflight"
                .into(),
        );
    }
    let selected_tags = plan.cfg.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    if selected_tags != expected_tags {
        return Err(format!(
            "only bracket: selected IDs changed: expected={expected_tags:?} actual={selected_tags:?}"
        ));
    }
    let selected_child = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == child)
        .ok_or("only bracket: selected child disappeared")?;
    if !selected_child.deps.contains(&parent) {
        return Err("only bracket: an edge among requested IDs was dropped".into());
    }
    if plan.cfg.steps.iter().any(|step| {
        step.group == "shard"
            || step.cmd.contains("ci/run-node.sh")
            || step.cmd.contains("dagrun run")
    }) {
        return Err("only bracket: selected committed IDs introduced a nested scheduler".into());
    }
    require_committed_scheduler_input(&plan)?;

    let original_command = plan.cfg.steps[0].cmd.clone();
    plan.cfg.steps[0].cmd.push_str(" --planted-runtime-remix");
    let mutation_error = require_committed_scheduler_input(&plan)
        .err()
        .ok_or("only bracket: planted selected-step mutation reached the scheduler boundary")?;
    if !mutation_error.contains("changed after selection") {
        return Err(format!(
            "only bracket: selected-step mutation refusal was not specific: {mutation_error}"
        ));
    }
    plan.cfg.steps[0].cmd = original_command;
    require_committed_scheduler_input(&plan)?;

    let off_record_args = parse_argv(&[
        ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into(),
        "--only".into(),
        "portable".into(),
        "test.detcore_unit".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: off-record selection failed with exit {code}"))?;
    let off_record = build_plan(root, &off_record_args, &std::env::temp_dir())?;
    let off_record_tags = off_record
        .cfg
        .steps
        .iter()
        .map(Step::tag)
        .collect::<BTreeSet<_>>();
    let expected_off_record = [
        "pre.submodules".to_string(),
        PIN_GATE_TAG.to_string(),
        "test.detcore_unit".to_string(),
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    if off_record_tags != expected_off_record {
        return Err(format!(
            "only bracket: off-record selection did not retain exactly requested ID plus minimal preflight: expected={expected_off_record:?} actual={off_record_tags:?}"
        ));
    }

    let unknown_args = parse_argv(&[
        "--only".into(),
        "portable".into(),
        "no.such_node".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: parser refused unknown-ID bracket with exit {code}"))?;
    let unknown = build_plan(root, &unknown_args, &std::env::temp_dir())
        .err()
        .ok_or("only bracket: unknown selected ID was accepted")?;
    if !unknown.contains("unknown step tag") || !unknown.contains("Known tags") {
        return Err(format!(
            "only bracket: unknown-ID refusal omitted its diagnosis or choices: {unknown}"
        ));
    }

    let privileged_args = parse_argv(&[
        "--only".into(),
        "privileged".into(),
        "cpuid.faulting".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: privileged public ID failed with exit {code}"))?;
    let privileged = build_plan(root, &privileged_args, &std::env::temp_dir())?;
    let privileged_tags = privileged
        .cfg
        .steps
        .iter()
        .map(Step::tag)
        .collect::<BTreeSet<_>>();
    if !privileged_tags.contains("privileged-only-cpuid.faulting")
        || privileged_tags.contains("cpuid.faulting")
    {
        return Err(format!(
            "only bracket: old privileged public ID did not resolve to its committed node: {privileged_tags:?}"
        ));
    }
    let privileged_unknown_args = parse_argv(&[
        "--only".into(),
        "privileged".into(),
        "no.such_privileged_node".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: privileged unknown-ID parser exit {code}"))?;
    let privileged_unknown = build_plan(root, &privileged_unknown_args, &std::env::temp_dir())
        .err()
        .ok_or("only bracket: unknown privileged public ID was accepted")?;
    if !privileged_unknown.contains("unknown step tag") {
        return Err(format!(
            "only bracket: privileged unknown-ID refusal lost its cause: {privileged_unknown}"
        ));
    }

    let unrelated_args = parse_argv(&[
        ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into(),
        "--only".into(),
        "portable".into(),
        "test.detcore_unit".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: unrelated selection was refused with exit {code}"))?;
    let unrelated_plan = build_plan(
        root,
        &unrelated_args,
        &std::env::temp_dir().join("validate-only-unrelated-plan"),
    )?;
    if unrelated_plan
        .cfg
        .steps
        .iter()
        .any(|step| step.job.starts_with("manifest_plan"))
    {
        return Err(
            "only bracket: unrelated test.detcore_unit selection admitted a manifest-plan producer"
                .into(),
        );
    }

    // Reproduce the focused manifest selection that used to reach dagrun with a
    // dangling pinned-root edge. `--only` intentionally drops unrelated build
    // dependencies, but both the host and pinned-root manifest commands invoke
    // target/debug/test-harness. A fresh checkout must therefore retain the
    // canonical host producer and add its in-image twin without restoring
    // gate.manifest.
    let manifest_args = parse_argv(&[
        ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into(),
        "--only".into(),
        "portable".into(),
        "build.manifest_guests,e2e.manifest_applications,e2e.manifest_c_programs,e2e.manifest_system_utils"
            .into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: manifest selection was refused with exit {code}"))?;
    let manifest_plan = build_plan(
        root,
        &manifest_args,
        &std::env::temp_dir().join("validate-only-manifest-plan"),
    )?;
    let manifest_tags: BTreeSet<String> =
        manifest_plan.cfg.steps.iter().map(|step| step.tag()).collect();
    for required in [
        "build.manifest_guests",
        "setup.manifest_plan",
        "setup.manifest_plan_in_pinned_root",
        "build.manifest_guests_in_pinned_root",
        "e2e.manifest_applications",
        "e2e.manifest_c_programs",
        "e2e.manifest_system_utils",
    ] {
        if !manifest_tags.contains(required) {
            return Err(format!(
                "only bracket: focused manifest selection omitted required node {required}: {manifest_tags:?}"
            ));
        }
    }
    if manifest_tags.contains("gate.manifest") || manifest_tags.contains("lint.clippy") {
        return Err(format!(
            "only bracket: focused manifest selection broadened into unrelated validation: {manifest_tags:?}"
        ));
    }
    let manifest_build = manifest_plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "build.manifest_guests")
        .ok_or("only bracket: focused manifest selection lost build.manifest_guests")?;
    if !manifest_build
        .deps
        .iter()
        .any(|dependency| dependency == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
    {
        return Err(
            "only bracket: host manifest build does not wait for setup.manifest_plan".into(),
        );
    }
    let pinned_manifest_build = manifest_plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "build.manifest_guests_in_pinned_root")
        .ok_or("only bracket: focused manifest selection lost pinned-root manifest build")?;
    if !pinned_manifest_build
        .deps
        .iter()
        .any(|dependency| dependency == "setup.manifest_plan_in_pinned_root")
    {
        return Err(
            "only bracket: pinned-root manifest build does not wait for its manifest-plan producer"
                .into(),
        );
    }
    for selected_cell in [
        "e2e.manifest_applications",
        "e2e.manifest_c_programs",
        "e2e.manifest_system_utils",
    ] {
        let step = manifest_plan
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == selected_cell)
            .ok_or_else(|| format!("only bracket: focused manifest selection lost {selected_cell}"))?;
        if !step
            .deps
            .iter()
            .any(|dependency| dependency == "build.manifest_guests_in_pinned_root")
        {
            return Err(format!(
                "only bracket: {selected_cell} does not wait for the pinned-root manifest build"
            ));
        }
    }
    let violations = dagrun::model::graph_structure_violations(&manifest_plan.cfg);
    if !violations.is_empty() {
        return Err(format!(
            "only bracket: focused manifest selection is not dependency-closed and schedulable: {violations:?}"
        ));
    }

    for (lane, target, producer, pin) in [
        ("hosted-portable", "build.manifest_guests", "setup.manifest_plan", PIN_GATE_TAG),
        ("hosted-privileged", "privileged-build.manifest_guests_on_host", "setup.manifest_plan_on_host", "pre.reverie_pin_on_host"),
    ] {
        for off_record in [false, true] {
            let mut argv = vec!["--only".into(), lane.into(), target.into()];
            if off_record { argv.push(ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into()); }
            let args = parse_argv(&argv).map_err(|code| format!("only bracket: {lane} parser exited {code}"))?;
            let hosted = build_plan(root, &args, &std::env::temp_dir())?;
            let tags = hosted.cfg.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
            if !tags.contains(target) || !tags.contains(producer) || !tags.contains(pin)
                || tags.iter().any(|tag| tag.ends_with("_in_pinned_root") || tag == "setup.pinned_root_fetch")
                || (off_record && tags.iter().any(|tag| tag.starts_with("gate.")))
            {
                return Err(format!("only bracket: {lane} changed focused host preparation (off_record={off_record}): {tags:?}"));
            }
            if !dagrun::model::graph_structure_violations(&hosted.cfg).is_empty() {
                return Err(format!("only bracket: {lane} focused host preparation is not dependency-closed"));
            }
        }
    }

    let source_after = std::fs::read(&source_path)
        .map_err(|error| format!("only bracket: cannot re-read source DAG: {error}"))?;
    if source_after != source_before {
        return Err("only bracket: selecting IDs changed ci/dag/validate.json".into());
    }
    println!(
        "  only plan: requested IDs plus committed preflight selected through dagrun; old privileged public IDs map explicitly, unknown portable and privileged IDs refuse, outside dependencies drop, selected edges remain, nested scheduler absent, and source/selected-step bytes are guarded both ways"
    );
    Ok(())
}

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
        "super-compatprep.fixtures",
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
        vec![dagrun::model::Step {
            group: "bracket".into(),
            job: "uncapped".into(),
            desc: "inert fixture: declares no caps".into(),
            description: String::new(),
            cmd: "true".into(),
            cmdtype: CmdType::Unknown,
            manifest: None,
            integration_test_binaries: None,
            result_manifests: None,
            labels: Vec::new(),
            deps: vec![],
            env: BTreeMap::new(),
            hint: Default::default(),
            networkonly: false,
            engine_only: false,
            timeout: 0,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
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
    let envelope = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "test.envelope_levels")
        .ok_or("verbosity: full plan lost test.envelope_levels")?;
    for fixture in [
        "run_probe true '/bin/true'",
        "run_probe echo '/bin/echo hermit-envelope'",
        "run_probe date '/bin/date -u +%Y'",
    ] {
        if !envelope.cmd.contains(fixture) {
            return Err(format!("verbosity: envelope lost stable identity fixture {fixture:?}"));
        }
    }
    if !envelope
        .cmd
        .contains("printf '##TEST-START %s\\n' \"$id\" >&2")
        || !envelope
            .cmd
            .contains("printf '##TEST-END %s PASS\\n' \"$id\" >&2")
    {
        return Err("verbosity: envelope START/END must use the same whitespace-free identity".into());
    }
    if envelope.cmd.matches("\"$id\" >&2").count() != 2
        || envelope.cmd.matches("</dev/null >&2").count() != 4
    {
        return Err(
            "verbosity: envelope markers and Hermit diagnostics must share stderr ordering".into(),
        );
    }
    for fixture in [
        "trap publish_counts EXIT",
        "EXECUTED=$((EXECUTED + 1))",
        "RESULTS+=(\"envelope/$id\" pass 1)",
        "RESULTS+=(\"envelope/$CURRENT_TEST\" fail 1)",
        "./ci/write-structured-test-counts.sh \"$EXECUTED\" 0 \"${RESULTS[@]}\"",
    ] {
        if !envelope.cmd.contains(fixture) {
            return Err(format!(
                "verbosity: envelope lost structured count fixture {fixture:?}"
            ));
        }
    }
    let non_nextest_test_nodes = plan
        .cfg
        .steps
        .iter()
        .chain(plan.second.iter().flat_map(|cfg| cfg.steps.iter()))
        .filter(|step| step.group == "test" && !step.cmd.contains("run-nextest-counted.sh"))
        .map(|step| step.tag())
        .collect::<BTreeSet<_>>();
    let expected_non_nextest = BTreeSet::from([
        "test.applications_e2e".to_string(),
        "test.dbt_parity".to_string(),
        "test.envelope_levels".to_string(),
    ]);
    if non_nextest_test_nodes != expected_non_nextest {
        return Err(format!(
            "verbosity: non-nextest test nodes changed without a structured-count audit: \
             {non_nextest_test_nodes:?}"
        ));
    }
    for (relative, marker) in [
        (
            "tests/e2e/lib/applications/run_all.sh",
            "write-structured-test-counts.sh",
        ),
        (
            "tests/backend-parity/run_matrix.py",
            "DAGRUN_TEST_COUNTS_PATH",
        ),
    ] {
        let source = std::fs::read_to_string(root.join(relative))
            .map_err(|error| format!("verbosity: cannot read {relative}: {error}"))?;
        if !source.contains(marker) {
            return Err(format!(
                "verbosity: {relative} no longer publishes structured test counts"
            ));
        }
    }
    let pinned_root_wrapper = std::fs::read_to_string(root.join("ci/hermetic/run-in-pinned-root.sh"))
        .map_err(|error| format!("verbosity: cannot read pinned-root wrapper: {error}"))?;
    for fixture in [
        "DAGRUN_TEST_COUNTS_PATH)",
        "destination=/dagrun-test-counts",
        "DAGRUN_TEST_COUNTS_PATH=/dagrun-test-counts/$counts_file",
    ] {
        if !pinned_root_wrapper.contains(fixture) {
            return Err(format!(
                "verbosity: pinned-root wrapper lost structured count mapping {fixture:?}"
            ));
        }
    }
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

/// A plan-only invocation executes no nodes and establishes no run deadline.
///
/// In particular, it may be called from a node of an enclosing validation and
/// inherit that run's timeout environment. Applying the inherited execution
/// budget to the raw, pre-wrapping plan makes an inert inventory query refuse
/// even though the enclosing execution has already clamped its runnable nodes
/// to the time remaining. An explicit `--run-timeout` still audits the raw plan
/// by request. Real executions keep the timeout unchanged and still fail closed
/// on an impossible budget.
fn effective_run_timeout(
    explicit: Option<i64>,
    inherited: Option<i64>,
    show_plan: bool,
) -> Option<i64> {
    if show_plan {
        explicit
    } else {
        explicit.or(inherited)
    }
}

/// The wall ceiling every node must fit inside, DERIVED from the seconds left on
/// the run epoch rather than written beside the nominal budget.
///
/// The scheduler refuses any node whose declared wall is `>=` the budget it is
/// enforcing, and what it enforces is the REMAINDER, not the nominal figure. A
/// ceiling that is merely smaller than the nominal budget therefore inverts once
/// preparation has spent enough of the epoch. Keeping one grace band below the
/// remainder makes that inversion unreachable at any preparation time.
fn derived_wall_ceiling(remaining_s: i64) -> i64 {
    (remaining_s - scope_grace_s(remaining_s)).max(1)
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
    service_result_path: Option<&Path>,
) -> Result<BoxedCgroups, u8> {
    let owns_request = owns_scope_request(deadline_ns);
    if is_in_scope() && run_timeout_s.is_some() && !owns_request {
        eprintln!(
            "validate: inherited cgroup scope has no invocation-owned RuntimeMaxSec rung; \
             the anchored in-process deadline remains inside the enclosing DAG node limit"
        );
    }
    let scope_runtime_s = run_timeout_s.and_then(|run| {
        remaining_budget_s(deadline_ns).map(|remaining| remaining + scope_grace_s(run))
    });
    if !allow_failure {
        if let Some(deadline) = deadline_ns {
            std::env::set_var(OWN_SCOPE_DEADLINE_ENV, deadline.to_string());
        } else {
            std::env::remove_var(OWN_SCOPE_DEADLINE_ENV);
        }
    }
    // `main` removes this producer-owned path immediately after capturing it so
    // nested commands cannot inherit authority to publish a competing result.
    // The one legitimate descendant is our own systemd scope replacement.
    // Expose the stored path only across that exec boundary; if setup returns,
    // remove it again before any DAG payload can be launched.
    if let Some(path) = service_result_path {
        std::env::set_var(VALIDATE_SERVICE_RESULT_PATH_ENV, path);
    }
    let result = safe_ci_scope::resolve_cgroups(
        "validate",
        allow_failure,
        scope_runtime_s,
        owns_request,
    );
    std::env::remove_var(VALIDATE_SERVICE_RESULT_PATH_ENV);
    safe_ci_scope::propagate_result(result)
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
    let supplied = std::env::var("E2E_RUN_ID")
        .ok()
        .filter(|value| {
            !value.is_empty()
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._@:-".contains(c))
        });
    let run = supplied.unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        format!(
            "{}-{}-{nanos}",
            utc_now().replace([':', '-'], ""),
            std::process::id()
        )
    });
    durable_log_path_for_run(&dir, profile, &sha12, &run)
}

fn durable_log_path_for_run(dir: &Path, profile: &str, sha12: &str, run: &str) -> PathBuf {
    dir.join(format!("validate-{profile}-{sha12}-{run}.log"))
}

fn fallback_e2e_result_root(log_path: &Path, run: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let log_dir = log_path
        .parent()
        .ok_or_else(|| format!("durable log has no parent: {}", log_path.display()))?;
    Ok(log_dir.join("e2e").join(run))
}

/// Give every real validate invocation its own durable E2E result directory.
///
/// `target/debug/test-harness` already emits one schema-4 row per cell, but its local
/// default is under the checkout. A canonical validate may run in a disposable
/// scratch tree, so those rows disappeared at cleanup. Deriving the fallback
/// from the durable log puts both artifacts under the same surviving root. A
/// caller such as ci-hub may still provide an explicit per-run location.
fn configure_e2e_result_root(
    root: &Path,
    log_path: &Path,
    temporary_build_root: &Path,
) -> Result<PathBuf, String> {
    let fallback_run = log_path
        .file_stem()
        .ok_or_else(|| format!("durable log has no file name: {}", log_path.display()))?
        .to_os_string();
    let run = std::env::var_os("E2E_RUN_ID")
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_run);
    let path = match std::env::var_os("E2E_RESULT_ROOT") {
        Some(value) if !value.is_empty() => {
            let supplied = PathBuf::from(value);
            if supplied.is_absolute() {
                supplied
            } else {
                root.join(supplied)
            }
        }
        _ => fallback_e2e_result_root(log_path, &run)?,
    };
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("cannot create E2E result directory {}: {e}", path.display()))?;
    std::env::set_var("E2E_RESULT_ROOT", &path);
    // One full validate invokes the harness once per manifest bucket. Bind all
    // bucket rows to the durable validate identity instead of letting each
    // harness process mint a local timestamp. Schema-7 evidence is one complete
    // selected population, not a pool of unrelated bucket attempts.
    std::env::set_var("E2E_RUN_ID", &run);
    // The harness derives its prebuilt-fixture directory from RESULT_ROOT too,
    // but build products are not evidence and must not accumulate beside every
    // retained scorecard. Keep them in validate's ordinary disposable run
    // directory unless the caller deliberately supplied a build root.
    if std::env::var_os("E2E_BUILD_ROOT").is_none() {
        std::fs::create_dir_all(temporary_build_root).map_err(|e| {
            format!(
                "cannot create temporary E2E build directory {}: {e}",
                temporary_build_root.display()
            )
        })?;
        std::env::set_var("E2E_BUILD_ROOT", temporary_build_root);
    }
    Ok(path)
}

#[cfg(test)]
mod concurrent_validate_path_tests {
    use std::io::Write;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn durable_outputs_reproduce_the_old_second_collision_and_separate_runs_now() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hermit-validate-output-collision-{}-{nanos}",
            std::process::id()
        ));
        let logs = root.join("ignored/validate");
        std::fs::create_dir_all(&logs).unwrap();
        let sha12 = "0123456789ab";

        // Removed behavior: second-resolution identity made both runs append to
        // one log and write one retained E2E tree.
        let old = logs.join(format!("validate-full-{sha12}-20260826T120000Z.log"));
        std::fs::write(&old, b"run-a\n").unwrap();
        let mut old_second = std::fs::OpenOptions::new().append(true).open(&old).unwrap();
        old_second.write_all(b"run-b\n").unwrap();
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "run-a\nrun-b\n");
        let old_e2e = logs.join("e2e/validate-full-0123456789ab-20260826T120000Z");
        std::fs::create_dir_all(&old_e2e).unwrap();
        std::fs::write(old_e2e.join("cell-results.jsonl"), b"run-a").unwrap();
        std::fs::write(old_e2e.join("cell-results.jsonl"), b"run-b").unwrap();
        assert_eq!(
            std::fs::read(old_e2e.join("cell-results.jsonl")).unwrap(),
            b"run-b"
        );

        let log_a = durable_log_path_for_run(&logs, "full", sha12, "validate-a");
        let log_b = durable_log_path_for_run(&logs, "full", sha12, "validate-b");
        assert_ne!(log_a, log_b);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_a)
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_b)
            .unwrap();
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&log_a)
                .is_err(),
            "reusing one run identity must refuse rather than append"
        );
        let e2e_a = fallback_e2e_result_root(&log_a, std::ffi::OsStr::new("validate-a")).unwrap();
        let e2e_b = fallback_e2e_result_root(&log_b, std::ffi::OsStr::new("validate-b")).unwrap();
        assert_ne!(e2e_a, e2e_b);
        std::fs::create_dir_all(&e2e_a).unwrap();
        std::fs::create_dir_all(&e2e_b).unwrap();
        std::fs::write(e2e_a.join("cell-results.jsonl"), b"run-a").unwrap();
        std::fs::write(e2e_b.join("cell-results.jsonl"), b"run-b").unwrap();
        assert_eq!(
            std::fs::read(e2e_a.join("cell-results.jsonl")).unwrap(),
            b"run-a"
        );
        assert_eq!(
            std::fs::read(e2e_b.join("cell-results.jsonl")).unwrap(),
            b"run-b"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unavailable_checkout_lock_refuses_before_shared_target_output_is_written() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hermit-validate-unavailable-lock-{}-{nanos}",
            std::process::id()
        ));
        let shared = root.join("old-target/shared-output");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        let (first_written_tx, first_written_rx) = mpsc::sync_channel(0);
        let (second_written_tx, second_written_rx) = mpsc::sync_channel(0);
        let first_path = shared.clone();
        let first = std::thread::spawn(move || {
            std::fs::write(first_path, b"run-a").unwrap();
            first_written_tx.send(()).unwrap();
            second_written_rx.recv().unwrap();
        });
        let second_path = shared.clone();
        let second = std::thread::spawn(move || {
            first_written_rx.recv().unwrap();
            std::fs::write(second_path, b"run-b").unwrap();
            second_written_tx.send(()).unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(
            std::fs::read(&shared).unwrap(),
            b"run-b",
            "the removed fail-open path allowed the second run to replace the first run's target output"
        );

        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("target"), b"not a directory").unwrap();
        let error = match validate_runtime::acquire_invocation_lock(&checkout, "full", "abc") {
            validate_runtime::LockOutcome::Unavailable(error) => error,
            _ => panic!("an unusable target path must make the checkout lock unavailable"),
        };
        let summary = unavailable_invocation_lock_summary("full", error);
        assert_eq!(summary.verdict, Verdict::Refused);
        assert!(summary
            .detail
            .iter()
            .any(|line| line.contains("refusing rather than running two validates")));
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Send all completed cells from one validate invocation to the parent series
/// writer as one batch. The harness appends retries to each bucket's existing
/// `results.jsonl`, so this reads the same durable attempt records used by the
/// terminal-verdict projection instead of maintaining another result file.
fn append_validate_series(
    parent: Option<&Path>,
    tool_root: Option<&Path>,
    checkout: &Path,
    result_root: &Path,
    tree: &str,
) -> Result<bool, String> {
    let Some(parent) = parent else {
        return Ok(false);
    };
    let tool_root = tool_root.ok_or("dev-hermit state root has no executable tool root")?;
    let rows = validate_cell_results::all_result_rows(result_root)?;
    if rows.is_empty() {
        return Ok(false);
    }
    let run_id = std::env::var_os("E2E_RUN_ID")
        .filter(|value| !value.is_empty())
        .ok_or("E2E_RUN_ID is missing after completed cell rows were recorded")?;
    let script = tool_root.join("ci-hub/series/series.py");
    if !script.is_file() {
        return Err(format!(
            "{} does not exist; {TOOL_ROOT_ENV} does not contain the series writer",
            script.display()
        ));
    }
    let mut child = Command::new("python3")
        .arg(&script)
        .arg("append-cells")
        .arg("--parent")
        .arg(parent)
        .arg("--checkout")
        .arg(checkout)
        .arg("--producer")
        .arg("validate")
        .arg("--run-id")
        .arg(&run_id)
        .arg("--tree")
        .arg(tree)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", script.display()))?;
    {
        use std::io::Write;
        let input = child
            .stdin
            .as_mut()
            .ok_or_else(|| format!("{} has no writable stdin", script.display()))?;
        for row in &rows {
            serde_json::to_writer(&mut *input, row)
                .map_err(|error| format!("cannot encode retained cell row: {error}"))?;
            input
                .write_all(b"\n")
                .map_err(|error| format!("cannot send retained cell row: {error}"))?;
        }
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for {}: {error}", script.display()))?;
    if !output.status.success() {
        return Err(format!(
            "series writer refused {} retained cell row(s) from {}: {}",
            rows.len(),
            result_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    eprintln!(
        "validate: per-cell series updated from {} retained row(s) under {}: {}",
        rows.len(),
        result_root.display(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(true)
}

/// Merge one top-level validate's completed per-cell rows into the tracked
/// scorecard files. Nested and off-the-record validates leave the tracked view
/// untouched; only a receipt-producing top-level run owns that projection.
fn should_write_scorecard(nested: bool, off_the_record: bool) -> bool {
    !nested && !off_the_record
}

fn local_scorecard_writeback(
    root: &Path,
    result_root: &Path,
    nested: bool,
    off_the_record: bool,
) -> Option<Result<(), String>> {
    if !should_write_scorecard(nested, off_the_record) {
        return None;
    }
    let script = root.join("ci/compat-envelope/scorecard.rs");
    if !script.is_file() {
        return Some(Err(format!("{} does not exist", script.display())));
    }
    Some(
        Command::new(&script)
            .arg("observe-results")
            .arg("--results")
            .arg(result_root)
            .current_dir(root)
            .status()
            .map_err(|error| format!("cannot run {}: {error}", script.display()))
            .and_then(|status| {
                status.success().then_some(()).ok_or_else(|| {
                    format!("{} observe-results refused with {status}", script.display())
                })
            }),
    )
}

fn record_scorecard_writeback(
    summary: &mut RunSummary,
    writeback: Option<Result<(), String>>,
) {
    let Some(writeback) = writeback else { return };
    let detail = match writeback {
        Ok(()) => {
            summary.scorecard_writeback = Some(ScorecardWriteback::Completed);
            "scorecard write-back completed; review the generated SCORECARD.md and ci/compat-envelope/cells.json changes before committing".into()
        }
        Err(error) => {
            if summary.exit_code == 0 {
                summary.exit_code = COULD_NOT_RUN_EXIT_CODE;
            }
            summary.scorecard_writeback = Some(ScorecardWriteback::Failed {
                error: error.clone(),
            });
            format!(
                "scorecard write-back FAILED after validation evidence was finalized: {error}; the validation verdict above is unchanged"
            )
        }
    };
    summary.detail.push(detail);
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
    if let Err(e) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        eprintln!(
            "validate: ERROR: cannot reserve durable log {}: {e}. Refusing to append two runs to one path.",
            path.display()
        );
        return Err(4);
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

fn parse_git_depth(raw: &str) -> Result<u64, String> {
    let depth = raw
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("git rev-list returned a non-integer depth {raw:?}: {error}"))?;
    if depth == 0 {
        return Err("git rev-list returned zero depth for a commit".into());
    }
    Ok(depth)
}

/// Measure the exact quantity carried by the historical `git_depth` field.
/// Failure is a refusal, never a fabricated zero or an omitted JSON key.
fn measure_git_depth(commit: &str) -> Result<u64, String> {
    let output = Command::new("git")
        .args(["rev-list", "--count", commit])
        .output()
        .map_err(|error| format!("cannot execute git rev-list --count {commit}: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "git rev-list --count {commit} failed with {}{}",
            output.status,
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        ));
    }
    parse_git_depth(&String::from_utf8_lossy(&output.stdout))
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
    foreign_porcelain_at(Path::new("."), args)
}

fn foreign_porcelain_at(root: &Path, args: &[&str]) -> Vec<String> {
    let Ok(out) = Command::new("git").current_dir(root).args(args).output() else {
        return Vec::new();
    };
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
    tree_dirty_at(Path::new("."))
}

fn tree_dirty_at(root: &Path) -> bool {
    !foreign_porcelain_at(root, &["status", "--porcelain"]).is_empty()
}

/// True when the WORKING TREE proper carries changes `git add` would capture.
/// This drives the hard gate, because staging or committing is the caller's
/// escape from it.
fn worktree_dirty() -> bool {
    worktree_dirty_at(Path::new("."))
}

fn worktree_dirty_at(root: &Path) -> bool {
    let unstaged = !foreign_porcelain_at(root, &["diff", "--name-only"]).is_empty();
    unstaged
        || !foreign_porcelain_at(root, &["ls-files", "--others", "--exclude-standard"]).is_empty()
}

/// Whether this checkout is one of ci-hub's disposable validate worktrees.
///
/// The location and `validate-fresh-` boundary are the same evidence ci-hub's
/// cleanup path uses before removing a recorded temporary checkout. This path
/// fact is never sufficient admission by itself: the caller must separately
/// establish the live canonical validate-lock ancestry before treating a run as
/// out of place.
fn is_disposable_validate_checkout(root: &Path, parent: Option<&Path>) -> bool {
    let Some(parent) = parent else { return false };
    let Ok(relative) = root.strip_prefix(parent.join("worktrees/validate")) else {
        return false;
    };
    relative.components().count() == 1
        && relative
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("validate-fresh-"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CheckoutAdmission {
    lock_admitted: bool,
    disposable: bool,
}

fn checkout_admission(
    root: &Path,
    parent: Option<&Path>,
    tool_root: Option<&Path>,
    commit: &str,
    host: &str,
) -> CheckoutAdmission {
    let lock_admitted = validate_lock_admission(tool_root, commit, host).is_ok();
    CheckoutAdmission {
        lock_admitted,
        disposable: lock_admitted && is_disposable_validate_checkout(root, parent),
    }
}

/// Dirtiness relevant to source attribution.
///
/// An admitted disposable checkout is a materialized copy of the exact clean
/// source commit. Its later state is diagnostic: validation itself writes the
/// generated scorecard and Hermit may leave a private summary behind when a
/// process is killed. An in-place run has no such boundary, so its final tree
/// still participates in attribution. Dirt present before execution always
/// participates in both cases.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceAttribution {
    commit_anchored: bool,
    tree_dirty: bool,
}

fn source_attribution(
    commit: &str,
    dirty_at_admission: bool,
    dirty_after_run: bool,
    admitted_disposable_checkout: bool,
) -> SourceAttribution {
    let tree_dirty = dirty_at_admission || (!admitted_disposable_checkout && dirty_after_run);
    SourceAttribution {
        commit_anchored: commit != "unknown" && !tree_dirty,
        tree_dirty,
    }
}

fn dirty_worktree_requires_refusal(nested: bool, dirty: bool, skip_check: bool) -> bool {
    !nested && dirty && !skip_check
}

fn utc_now() -> String {
    sh("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".into())
}

fn epoch_now() -> i64 {
    sh("date", &["+%s"]).and_then(|s| s.parse().ok()).unwrap_or(0)
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

/// Capability emitted by the installed immutable wrapper after it has retained
/// both directories and verified the frozen bytes. This is deliberately a
/// same-UID provenance contract, not a privilege boundary against a process
/// that can already replace or impersonate that installed wrapper.
#[derive(Debug)]
struct ImmutableToolAuthority {
    holder_pid: u32,
    authority_fd: u32,
    target_fd: u32,
    target_root: PathBuf,
    target_dev: u64,
    target_ino: u64,
    root_fd: u32,
    root_dev: u64,
    root_ino: u64,
    state_fd: u32,
    state_root: PathBuf,
    state_dev: u64,
    state_ino: u64,
    content_sha256: String,
    parent_sha: String,
    hermit_sha: String,
    agent_utils_sha: String,
    bootstrap_sha256: String,
}

fn lowercase_hex(value: &str, digits: usize) -> bool {
    value.len() == digits
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn proc_fd_path(path: &Path) -> Option<(u32, u32)> {
    let components: Vec<&[u8]> = path.as_os_str().as_bytes().split(|byte| *byte == b'/').collect();
    if components.len() != 5
        || !components[0].is_empty()
        || components[1] != b"proc"
        || components[3] != b"fd"
    {
        return None;
    }
    let parse = |value: &[u8]| -> Option<u32> {
        if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(value).ok()?.parse().ok()
    };
    let pid = parse(components[2])?;
    let fd = parse(components[4])?;
    (pid > 0).then_some((pid, fd))
}

fn authority_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<String, String> {
    object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("immutable tool authority has no string {field}"))
}

fn authority_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<u64, String> {
    object
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("immutable tool authority has no integer {field}"))
}

fn read_immutable_tool_authority(
    authority_path: &Path,
    tool_root: &Path,
    state_root: &Path,
) -> Result<ImmutableToolAuthority, String> {
    let (authority_pid, authority_fd_from_path) = proc_fd_path(authority_path).ok_or_else(|| {
        format!(
            "{TOOL_AUTHORITY_ENV} must be an exact /proc/<pid>/fd/<fd> capability"
        )
    })?;
    let (root_pid, root_fd_from_path) = proc_fd_path(tool_root).ok_or_else(|| {
        format!("{TOOL_ROOT_ENV} authority must be an exact /proc/<pid>/fd/<fd> capability")
    })?;
    if authority_pid != root_pid {
        return Err("immutable tool root and authority are owned by different holders".into());
    }

    let mut authority_file = std::fs::File::open(authority_path).map_err(|error| {
        format!(
            "cannot open immutable tool authority {}: {error}",
            authority_path.display()
        )
    })?;
    let authority_metadata = authority_file
        .metadata()
        .map_err(|error| format!("cannot inspect immutable tool authority: {error}"))?;
    if !authority_metadata.is_file()
        || authority_metadata.nlink() != 0
        || authority_metadata.mode() & 0o222 != 0
    {
        return Err(
            "immutable tool authority must be one anonymous read-only regular file".into(),
        );
    }
    let seals = unsafe { libc::fcntl(authority_file.as_raw_fd(), libc::F_GET_SEALS) };
    let required_seals = libc::F_SEAL_SEAL
        | libc::F_SEAL_SHRINK
        | libc::F_SEAL_GROW
        | libc::F_SEAL_WRITE;
    if seals < 0 || seals & required_seals != required_seals {
        return Err("immutable tool authority is not completely sealed".into());
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut authority_file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read immutable tool authority: {error}"))?;
    if bytes.len() > 4096 {
        return Err("immutable tool authority exceeds 4096 bytes".into());
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("immutable tool authority is not valid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or("immutable tool authority must be one JSON object")?;
    let expected_fields: BTreeSet<&str> = [
        "agent_utils_sha",
        "authority_fd",
        "bootstrap_sha256",
        "content_sha256",
        "hermit_sha",
        "holder_pid",
        "parent_sha",
        "root_dev",
        "root_fd",
        "root_ino",
        "schema",
        "state_dev",
        "state_fd",
        "state_ino",
        "state_root",
        "target_dev",
        "target_fd",
        "target_ino",
        "target_root",
    ]
    .into_iter()
    .collect();
    let observed_fields: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    if observed_fields != expected_fields {
        return Err("immutable tool authority fields are incomplete or unknown".into());
    }
    if authority_string(object, "schema")? != TOOL_AUTHORITY_SCHEMA {
        return Err("immutable tool authority has an unsupported schema".into());
    }

    let narrow_u32 = |field: &str| -> Result<u32, String> {
        u32::try_from(authority_u64(object, field)?)
            .map_err(|_| format!("immutable tool authority {field} is out of range"))
    };
    let authority = ImmutableToolAuthority {
        holder_pid: narrow_u32("holder_pid")?,
        authority_fd: narrow_u32("authority_fd")?,
        target_fd: narrow_u32("target_fd")?,
        target_root: PathBuf::from(authority_string(object, "target_root")?),
        target_dev: authority_u64(object, "target_dev")?,
        target_ino: authority_u64(object, "target_ino")?,
        root_fd: narrow_u32("root_fd")?,
        root_dev: authority_u64(object, "root_dev")?,
        root_ino: authority_u64(object, "root_ino")?,
        state_fd: narrow_u32("state_fd")?,
        state_root: PathBuf::from(authority_string(object, "state_root")?),
        state_dev: authority_u64(object, "state_dev")?,
        state_ino: authority_u64(object, "state_ino")?,
        content_sha256: authority_string(object, "content_sha256")?,
        parent_sha: authority_string(object, "parent_sha")?,
        hermit_sha: authority_string(object, "hermit_sha")?,
        agent_utils_sha: authority_string(object, "agent_utils_sha")?,
        bootstrap_sha256: authority_string(object, "bootstrap_sha256")?,
    };
    if authority.holder_pid != authority_pid
        || authority.authority_fd != authority_fd_from_path
        || authority.root_fd != root_fd_from_path
    {
        return Err("immutable tool authority does not name the supplied descriptors".into());
    }
    let descriptor_ids: BTreeSet<u32> = [
        authority.authority_fd,
        authority.target_fd,
        authority.root_fd,
        authority.state_fd,
    ]
    .into_iter()
    .collect();
    if descriptor_ids.len() != 4 {
        return Err(
            "immutable tool authority conflates target, executable, state, or record descriptors"
                .into(),
        );
    }
    for (label, value, digits) in [
        ("content digest", authority.content_sha256.as_str(), 64),
        ("parent SHA", authority.parent_sha.as_str(), 40),
        ("Hermit SHA", authority.hermit_sha.as_str(), 40),
        ("agent-utils SHA", authority.agent_utils_sha.as_str(), 40),
        ("bootstrap digest", authority.bootstrap_sha256.as_str(), 64),
    ] {
        if !lowercase_hex(value, digits) {
            return Err(format!("immutable tool authority has an invalid {label}"));
        }
    }

    let held_target = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        authority.holder_pid, authority.target_fd
    ));
    let target_metadata = std::fs::metadata(&held_target)
        .map_err(|error| format!("cannot inspect cached target descriptor: {error}"))?;
    if !target_metadata.is_dir()
        || target_metadata.mode() & 0o222 != 0
        || target_metadata.nlink() < 1
        || (target_metadata.dev(), target_metadata.ino())
            != (authority.target_dev, authority.target_ino)
    {
        return Err("cached target descriptor identity does not match its authority".into());
    }
    let resolved_target = std::fs::canonicalize(&authority.target_root).map_err(|error| {
        format!(
            "cannot resolve cached target authority {}: {error}",
            authority.target_root.display()
        )
    })?;
    if !authority.target_root.is_absolute()
        || resolved_target != authority.target_root
        || authority.target_root.file_name() != Some(OsStr::new(&authority.parent_sha))
        || authority.target_root.parent().and_then(Path::file_name) != Some(OsStr::new("trees"))
    {
        return Err("cached target authority does not name canonical trees/<parent-sha>".into());
    }
    let live_target_metadata = std::fs::symlink_metadata(&resolved_target)
        .map_err(|error| format!("cannot inspect canonical cached target: {error}"))?;
    if !live_target_metadata.is_dir()
        || (live_target_metadata.dev(), live_target_metadata.ino())
            != (authority.target_dev, authority.target_ino)
    {
        return Err("cached target pathname no longer names the retained authority".into());
    }
    let held_target_path = std::fs::read_link(&held_target)
        .map_err(|error| format!("cannot read cached target descriptor: {error}"))?;
    if held_target_path != authority.target_root {
        return Err("cached target descriptor no longer names its canonical path".into());
    }

    let root_metadata = std::fs::metadata(tool_root)
        .map_err(|error| format!("cannot inspect retained immutable tool root: {error}"))?;
    if !root_metadata.is_dir()
        || (root_metadata.dev(), root_metadata.ino()) != (authority.root_dev, authority.root_ino)
    {
        return Err("immutable tool root descriptor identity does not match its authority".into());
    }

    let resolved_state = std::fs::canonicalize(state_root).map_err(|error| {
        format!(
            "cannot resolve canonical {PARENT_ENV} {}: {error}",
            state_root.display()
        )
    })?;
    if resolved_state != authority.state_root {
        return Err(format!(
            "canonical {PARENT_ENV} {} does not match immutable tool authority state root {}",
            resolved_state.display(),
            authority.state_root.display()
        ));
    }
    let held_state = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        authority.holder_pid, authority.state_fd
    ));
    let held_state_metadata = std::fs::metadata(&held_state)
        .map_err(|error| format!("cannot inspect retained state-root descriptor: {error}"))?;
    if !held_state_metadata.is_dir()
        || (held_state_metadata.dev(), held_state_metadata.ino())
            != (authority.state_dev, authority.state_ino)
    {
        return Err("retained state-root descriptor identity does not match its authority".into());
    }
    let live_state_metadata = std::fs::metadata(&resolved_state)
        .map_err(|error| format!("cannot inspect canonical state root: {error}"))?;
    if (live_state_metadata.dev(), live_state_metadata.ino())
        != (authority.state_dev, authority.state_ino)
    {
        return Err("canonical state-root pathname no longer names the retained authority".into());
    }
    let held_state_target = std::fs::read_link(&held_state)
        .map_err(|error| format!("cannot read retained state-root descriptor: {error}"))?;
    if held_state_target != authority.state_root {
        return Err("retained state-root descriptor no longer names its canonical path".into());
    }
    let directory_identities: BTreeSet<(u64, u64)> = [
        (authority.target_dev, authority.target_ino),
        (authority.root_dev, authority.root_ino),
        (authority.state_dev, authority.state_ino),
    ]
    .into_iter()
    .collect();
    if directory_identities.len() != 3 {
        return Err(
            "immutable tool authority conflates target, executable, or state identity".into(),
        );
    }

    Ok(authority)
}

fn digest_length(hasher: &mut Sha256, value: usize) {
    hasher.update(u64::try_from(value).unwrap_or(u64::MAX).to_be_bytes());
}

fn digest_tool_entry(
    path: &Path,
    relative: &[u8],
    hasher: &mut Sha256,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect immutable tool entry {}: {error}", path.display()))?;
    let mode = metadata.mode() & 0o7777;
    let file_type = metadata.file_type();
    if file_type.is_dir() {
        if mode & 0o222 != 0 || metadata.nlink() < 1 {
            return Err(format!("immutable tool directory is writable or unlinked: {}", path.display()));
        }
        let held = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| format!("cannot retain immutable tool directory {}: {error}", path.display()))?;
        let held_metadata = held
            .metadata()
            .map_err(|error| format!("cannot inspect retained tool directory: {error}"))?;
        if !held_metadata.is_dir()
            || (held_metadata.dev(), held_metadata.ino()) != (metadata.dev(), metadata.ino())
        {
            return Err(format!("immutable tool directory changed before hashing: {}", path.display()));
        }
        hasher.update(b"d");
        digest_length(hasher, relative.len());
        hasher.update(relative);
        hasher.update(mode.to_be_bytes());
        let mut entries = std::fs::read_dir(path)
            .map_err(|error| format!("cannot list immutable tool directory {}: {error}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot list immutable tool directory {}: {error}", path.display()))?;
        entries.sort_by(|left, right| {
            left.file_name()
                .as_os_str()
                .as_bytes()
                .cmp(right.file_name().as_os_str().as_bytes())
        });
        for entry in entries {
            let name = entry.file_name();
            let name_bytes = name.as_os_str().as_bytes();
            let mut child_relative = relative.to_vec();
            if !child_relative.is_empty() {
                child_relative.push(b'/');
            }
            child_relative.extend_from_slice(name_bytes);
            if matches!(
                child_relative.as_slice(),
                b".git" | b"hermit/.git" | b"hermit/agent-utils/.git"
            ) {
                continue;
            }
            digest_tool_entry(&entry.path(), &child_relative, hasher)?;
        }
        let after = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot recheck immutable tool directory: {error}"))?;
        if (after.dev(), after.ino()) != (held_metadata.dev(), held_metadata.ino()) {
            return Err(format!("immutable tool directory changed while hashing: {}", path.display()));
        }
    } else if file_type.is_file() {
        if mode & 0o222 != 0 || metadata.nlink() != 1 {
            return Err(format!("immutable tool file is writable or multiply linked: {}", path.display()));
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| format!("cannot open immutable tool file {}: {error}", path.display()))?;
        let held_metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect retained tool file: {error}"))?;
        if !held_metadata.is_file()
            || (held_metadata.dev(), held_metadata.ino(), held_metadata.len())
                != (metadata.dev(), metadata.ino(), metadata.len())
        {
            return Err(format!("immutable tool file changed before hashing: {}", path.display()));
        }
        hasher.update(b"f");
        digest_length(hasher, relative.len());
        hasher.update(relative);
        hasher.update(mode.to_be_bytes());
        hasher.update(metadata.len().to_be_bytes());
        let mut observed = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| format!("cannot read immutable tool file {}: {error}", path.display()))?;
            if count == 0 {
                break;
            }
            observed = observed.saturating_add(count as u64);
            hasher.update(&buffer[..count]);
        }
        let after = file
            .metadata()
            .map_err(|error| format!("cannot recheck retained tool file: {error}"))?;
        if observed != metadata.len()
            || (after.dev(), after.ino(), after.len())
                != (held_metadata.dev(), held_metadata.ino(), held_metadata.len())
        {
            return Err(format!("immutable tool file changed while hashing: {}", path.display()));
        }
    } else if file_type.is_symlink() {
        let target = std::fs::read_link(path)
            .map_err(|error| format!("cannot read immutable tool symlink {}: {error}", path.display()))?;
        let target = target.as_os_str().as_bytes();
        hasher.update(b"l");
        digest_length(hasher, relative.len());
        hasher.update(relative);
        hasher.update(mode.to_be_bytes());
        digest_length(hasher, target.len());
        hasher.update(target);
    } else {
        return Err(format!("immutable tool has unsupported entry type at {}", path.display()));
    }
    Ok(())
}

fn immutable_tool_content_sha256(root: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(root)
        .map_err(|error| format!("cannot inspect immutable tool root {}: {error}", root.display()))?;
    if !metadata.is_dir() || metadata.mode() & 0o222 != 0 {
        return Err("immutable tool root must be one read-only directory".into());
    }
    let mut hasher = Sha256::new();
    hasher.update(b"dev-hermit-tool-content-v1\0");
    let mut entries = std::fs::read_dir(root)
        .map_err(|error| format!("cannot list immutable tool root: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot list immutable tool root: {error}"))?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_os_str()
            .as_bytes()
            .cmp(right.file_name().as_os_str().as_bytes())
    });
    for entry in entries {
        let name = entry.file_name();
        let relative = name.as_os_str().as_bytes();
        // Git metadata is not executable authority and is never consulted on
        // this admission path. Exclude only the three repository roots the
        // producer materializes; a nested `.git` anywhere else remains part of
        // the digest and cannot be smuggled in as a generic exception.
        if relative == b".git" {
            continue;
        }
        digest_tool_entry(&entry.path(), relative, &mut hasher)?;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn configured_authority_tool_root(
    supplied: &Path,
    state_root: &Path,
) -> Result<PathBuf, String> {
    let authority_value = std::env::var_os(TOOL_AUTHORITY_ENV)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("immutable tool authority is incomplete: {TOOL_AUTHORITY_ENV} is absent"))?;
    let authority_path = PathBuf::from(authority_value);
    if !authority_path.is_absolute() {
        return Err(format!("{TOOL_AUTHORITY_ENV} must be absolute"));
    }
    let authority = read_immutable_tool_authority(&authority_path, supplied, state_root)?;
    let required_environment = [
        (TOOL_CONTENT_SHA256_ENV, authority.content_sha256.as_str()),
        (TOOL_PARENT_SHA_ENV, authority.parent_sha.as_str()),
        (TOOL_HERMIT_SHA_ENV, authority.hermit_sha.as_str()),
        (TOOL_AGENT_UTILS_SHA_ENV, authority.agent_utils_sha.as_str()),
        (TOOL_BOOTSTRAP_SHA256_ENV, authority.bootstrap_sha256.as_str()),
    ];
    for (name, expected) in required_environment {
        let observed = std::env::var(name)
            .map_err(|_| format!("immutable tool authority is incomplete: {name} is absent"))?;
        if observed != expected {
            return Err(format!("{name} does not match immutable tool authority"));
        }
    }
    if !supplied.join("ci-hub").is_dir() {
        return Err(format!(
            "explicit {TOOL_ROOT_ENV} {} has no ci-hub directory",
            supplied.display()
        ));
    }
    let observed_digest = immutable_tool_content_sha256(supplied)?;
    if observed_digest != authority.content_sha256 {
        return Err(format!(
            "immutable tool content digest is {observed_digest}, expected {}",
            authority.content_sha256
        ));
    }
    Ok(supplied.to_path_buf())
}

struct SelfTestToolAuthority {
    _target: std::fs::File,
    _root: std::fs::File,
    _state: std::fs::File,
    _authority: std::fs::File,
    tool_root: PathBuf,
    authority_path: PathBuf,
    content_sha256: String,
}

fn make_self_test_tree_read_only(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("tool authority self-test cannot inspect tree: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)
            .map_err(|error| format!("tool authority self-test cannot list tree: {error}"))?
        {
            let entry = entry.map_err(|error| {
                format!("tool authority self-test cannot read directory entry: {error}")
            })?;
            make_self_test_tree_read_only(&entry.path())?;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555))
            .map_err(|error| format!("tool authority self-test cannot freeze directory: {error}"))?;
    } else if metadata.is_file() {
        let mode = if metadata.mode() & 0o111 != 0 { 0o555 } else { 0o444 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("tool authority self-test cannot freeze file: {error}"))?;
    }
    Ok(())
}

fn create_self_test_tool_authority(
    root: &Path,
    state_root: &Path,
    target_root: &Path,
    content_override: Option<&str>,
    wrong_root_inode: bool,
    wrong_target_inode: bool,
    target_fd_override: Option<&std::fs::File>,
) -> Result<SelfTestToolAuthority, String> {
    let target_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(target_root)
        .map_err(|error| format!("tool authority self-test cannot retain target: {error}"))?;
    let root_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|error| format!("tool authority self-test cannot retain root: {error}"))?;
    let resolved_state = std::fs::canonicalize(state_root)
        .map_err(|error| format!("tool authority self-test cannot resolve state: {error}"))?;
    let state_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&resolved_state)
        .map_err(|error| format!("tool authority self-test cannot retain state: {error}"))?;
    let name = std::ffi::CString::new("dev-hermit-tool-authority")
        .map_err(|error| format!("tool authority self-test memfd name failed: {error}"))?;
    let raw_authority = unsafe {
        libc::memfd_create(
            name.as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw_authority < 0 {
        return Err(format!(
            "tool authority self-test cannot create memfd: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut authority_file = unsafe { std::fs::File::from_raw_fd(raw_authority) };
    let root_metadata = root_file
        .metadata()
        .map_err(|error| format!("tool authority self-test cannot inspect root: {error}"))?;
    let state_metadata = state_file
        .metadata()
        .map_err(|error| format!("tool authority self-test cannot inspect state: {error}"))?;
    let target_metadata = target_file
        .metadata()
        .map_err(|error| format!("tool authority self-test cannot inspect target: {error}"))?;
    let digest = immutable_tool_content_sha256(root)?;
    let recorded_digest = content_override.unwrap_or(&digest);
    let holder_pid = std::process::id();
    let root_fd = u32::try_from(root_file.as_raw_fd())
        .map_err(|_| "tool authority self-test root fd is negative")?;
    let state_fd = u32::try_from(state_file.as_raw_fd())
        .map_err(|_| "tool authority self-test state fd is negative")?;
    let target_fd = u32::try_from(
        target_fd_override
            .unwrap_or(&target_file)
            .as_raw_fd(),
    )
    .map_err(|_| "tool authority self-test target fd is negative")?;
    let authority_fd = u32::try_from(authority_file.as_raw_fd())
        .map_err(|_| "tool authority self-test authority fd is negative")?;
    let record = serde_json::json!({
        "schema": TOOL_AUTHORITY_SCHEMA,
        "holder_pid": holder_pid,
        "authority_fd": authority_fd,
        "target_fd": target_fd,
        "target_root": target_root,
        "target_dev": target_metadata.dev(),
        "target_ino": target_metadata.ino() + if wrong_target_inode { 1 } else { 0 },
        "root_fd": root_fd,
        "root_dev": root_metadata.dev(),
        "root_ino": root_metadata.ino() + if wrong_root_inode { 1 } else { 0 },
        "state_fd": state_fd,
        "state_root": resolved_state,
        "state_dev": state_metadata.dev(),
        "state_ino": state_metadata.ino(),
        "content_sha256": recorded_digest,
        "parent_sha": "1111111111111111111111111111111111111111",
        "hermit_sha": "2222222222222222222222222222222222222222",
        "agent_utils_sha": "3333333333333333333333333333333333333333",
        "bootstrap_sha256": "4444444444444444444444444444444444444444444444444444444444444444",
    });
    let mut encoded = serde_json::to_vec(&record)
        .map_err(|error| format!("tool authority self-test cannot encode record: {error}"))?;
    encoded.push(b'\n');
    authority_file
        .write_all(&encoded)
        .map_err(|error| format!("tool authority self-test cannot write record: {error}"))?;
    authority_file
        .set_permissions(std::fs::Permissions::from_mode(0o400))
        .map_err(|error| format!("tool authority self-test cannot freeze record: {error}"))?;
    let seals = libc::F_SEAL_SEAL
        | libc::F_SEAL_SHRINK
        | libc::F_SEAL_GROW
        | libc::F_SEAL_WRITE;
    if unsafe { libc::fcntl(authority_file.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(format!(
            "tool authority self-test cannot seal record: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(SelfTestToolAuthority {
        tool_root: PathBuf::from(format!("/proc/{holder_pid}/fd/{root_fd}")),
        authority_path: PathBuf::from(format!("/proc/{holder_pid}/fd/{authority_fd}")),
        content_sha256: digest,
        _target: target_file,
        _root: root_file,
        _state: state_file,
        _authority: authority_file,
    })
}

fn install_self_test_tool_authority(authority: &SelfTestToolAuthority) {
    // SAFETY: callers use this only inside validate's single-threaded self-test
    // and restore every value before returning.
    unsafe {
        std::env::set_var(TOOL_ROOT_ENV, &authority.tool_root);
        std::env::set_var(TOOL_AUTHORITY_ENV, &authority.authority_path);
        std::env::set_var(TOOL_CONTENT_SHA256_ENV, &authority.content_sha256);
        std::env::set_var(
            TOOL_PARENT_SHA_ENV,
            "1111111111111111111111111111111111111111",
        );
        std::env::set_var(
            TOOL_HERMIT_SHA_ENV,
            "2222222222222222222222222222222222222222",
        );
        std::env::set_var(
            TOOL_AGENT_UTILS_SHA_ENV,
            "3333333333333333333333333333333333333333",
        );
        std::env::set_var(
            TOOL_BOOTSTRAP_SHA256_ENV,
            "4444444444444444444444444444444444444444444444444444444444444444",
        );
    }
}

/// Resolve executable ci-hub code separately from the canonical parent that
/// owns locks, ledgers, and other shared state.
///
/// Older direct callers supplied only `DEV_HERMIT_PARENT`, so absence retains
/// that behavior. An explicit tool root comes from the admitted launcher and
/// must be an absolute, existing dev-hermit checkout; silently falling back to
/// the state root would execute whatever code happens to be in the primary
/// checkout instead of the code that admitted this run.
fn configured_tool_root(parent: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let Some(value) = std::env::var_os(TOOL_ROOT_ENV) else {
        return Ok(parent.map(Path::to_path_buf));
    };
    if value.is_empty() {
        return Err(format!("{TOOL_ROOT_ENV} is explicitly empty"));
    }
    let supplied = PathBuf::from(value);
    if !supplied.is_absolute() {
        return Err(format!(
            "{TOOL_ROOT_ENV} must be absolute, got {}",
            supplied.display()
        ));
    }
    let authority_requested = std::env::var_os(TOOL_AUTHORITY_ENV).is_some()
        || std::env::var_os(TOOL_CONTENT_SHA256_ENV).is_some();
    if authority_requested {
        let state_root = parent.ok_or_else(|| {
            format!("explicit {TOOL_ROOT_ENV} has no canonical {PARENT_ENV} to bind to")
        })?;
        return configured_authority_tool_root(&supplied, state_root).map(Some);
    }
    let resolved = std::fs::canonicalize(&supplied).map_err(|error| {
        format!(
            "cannot resolve explicit {TOOL_ROOT_ENV} {}: {error}",
            supplied.display()
        )
    })?;
    if !resolved.join("ci-hub").is_dir() {
        return Err(format!(
            "explicit {TOOL_ROOT_ENV} {} has no ci-hub directory",
            resolved.display()
        ));
    }
    let state_root = parent.ok_or_else(|| {
        format!("explicit {TOOL_ROOT_ENV} has no canonical {PARENT_ENV} to bind to")
    })?;
    let git = |root: &Path, args: &[&str]| -> Result<String, String> {
        let output = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .args(["--no-optional-locks", "-C"])
            .arg(root)
            .args(args)
            .output()
            .map_err(|error| format!("cannot inspect {}: {error}", root.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{} is not a readable Git worktree: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let top = std::fs::canonicalize(git(&resolved, &["rev-parse", "--show-toplevel"])?)
        .map_err(|error| format!("cannot resolve tool worktree top level: {error}"))?;
    if top != resolved {
        return Err(format!(
            "explicit {TOOL_ROOT_ENV} {} is inside {}, not its top level",
            resolved.display(),
            top.display()
        ));
    }
    let state = std::fs::canonicalize(state_root).map_err(|error| {
        format!("cannot resolve canonical {PARENT_ENV} {}: {error}", state_root.display())
    })?;
    let state_top = std::fs::canonicalize(git(&state, &["rev-parse", "--show-toplevel"])?)
        .map_err(|error| format!("cannot resolve state worktree top level: {error}"))?;
    if state_top != state {
        return Err(format!(
            "canonical {PARENT_ENV} {} is inside {}, not its top level",
            state.display(),
            state_top.display()
        ));
    }
    let common = |root: &Path| -> Result<PathBuf, String> {
        let path = git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        std::fs::canonicalize(&path)
            .map_err(|error| format!("cannot resolve Git common directory {path}: {error}"))
    };
    if common(&resolved)? != common(&state)? {
        return Err(format!(
            "explicit {TOOL_ROOT_ENV} {} is not a worktree of canonical {PARENT_ENV} {}",
            resolved.display(),
            state.display()
        ));
    }
    if git(
        &resolved,
        &["config", "-f", ".gitmodules", "--get", "submodule.hermit.path"],
    )? != "hermit"
    {
        return Err(format!(
            "explicit {TOOL_ROOT_ENV} {} is not a dev-hermit checkout",
            resolved.display()
        ));
    }
    let dirty = git(
        &resolved,
        &["status", "--porcelain=v1", "--untracked-files=all", "--ignore-submodules=none"],
    )?;
    if !dirty.is_empty() {
        return Err(format!(
            "explicit {TOOL_ROOT_ENV} {} is dirty: {}",
            resolved.display(),
            dirty.lines().next().unwrap_or("unknown change")
        ));
    }
    Ok(Some(resolved))
}

/// Whether this invocation is real product work in a dev-hermit workspace and
/// therefore needs canonical ci-hub admission.
///
/// A `.gitmodules` entry alone can describe a generic Hermit superproject. The
/// ci-hub directory is the dev-hermit boundary; once that boundary exists, a
/// missing or broken launcher is non-authorizing rather than an escape.
fn product_front_door_applies(
    parent_detected: bool,
    ci_hub_dir_present: bool,
    _nested: bool,
    show_plan: bool,
) -> bool {
    parent_detected && ci_hub_dir_present && !show_plan
}

/// A local off-the-record run is an iteration tool, not a cheaper publication
/// path. It therefore requires both a commit anchor and an explicitly narrowed
/// profile. Full-cost validation and every publishable result stay in ci-hub.
fn local_off_the_record_refusal(args: &Args, dirty: bool) -> Option<String> {
    if !args.allow_local_off_the_record_run {
        return None;
    }
    if args.show_plan {
        return None;
    }
    if dirty {
        return Some(format!(
            "validate: REFUSED — {ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION} still requires a clean, \
             commit-anchored tree. Commit the work in progress first so this run records a SHA, \
             then retry the narrowed command."
        ));
    }
    if args.focused.is_none() && args.level != Level::Quick && args.selected.is_none() {
        return Some(format!(
            "validate: REFUSED — {ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION} is only for quick or \
             focused iterative testing. A full-cost validate belongs in ci-hub.\n\
             Example (one step, one test node ID):\n\n  \
             ./scripts/validate.rs {ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION} --only portable test.cli"
        ));
    }
    None
}

/// Construct the refusal for an unadmitted product run. Production supplies
/// `lock_admitted` only from [`validate_lock_admission`].
fn product_front_door_refusal(
    tool_root: &Path,
    root: &Path,
    commit: &str,
    requested_args: &str,
    ci_hub_launcher_available: bool,
    lock_admitted: bool,
) -> Option<String> {
    if lock_admitted {
        return None;
    }
    let ci_hub_path = tool_root.join("ci-hub/ci-hub");
    let ci_hub = validate_plan::shell_quote(&ci_hub_path.to_string_lossy());
    let checkout = validate_plan::shell_quote(&root.to_string_lossy());
    let remediation = if ci_hub_launcher_available {
        format!(
            "Publishing because the code is ready requires ci-hub:\n\n  {ci_hub} validate-run --checkout \
             {checkout} --agent '<registered-agent-name>' --target {commit} -- {requested_args}"
        )
    } else {
        format!(
            "The canonical ci-hub launcher is unavailable at {ci_hub}. Repair or sync the parent \
             checkout before publishing validation evidence."
        )
    };
    Some(format!(
        "validate: REFUSED — choose whether this is iterative testing or publishing evidence.\n\
         A direct run from {checkout} is not admitted to publish evidence.\n\
         \n\
         {remediation}\n\
         \n\
         Iterative testing must be narrow and off the record; its result cannot be cited as \
         validation evidence. Commit the work in progress first, then run one step by test node \
         ID, for example:\n\n  \
         ./scripts/validate.rs {ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION} --only portable test.cli"
    ))
}

/// Reproduce the caller's validated argv after ci-hub's `--` separator. An
/// empty argv means the driver's default full profile.
fn requested_validate_args() -> String {
    let args = std::env::args()
        .skip(1)
        .map(|arg| validate_plan::shell_quote(&arg))
        .collect::<Vec<_>>();
    if args.is_empty() {
        "full".into()
    } else {
        args.join(" ")
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
/// ERROR if the base is out of date." The reason is not tidiness: validation
/// records exact-commit evidence, and admission requires that commit to include
/// every `origin/main` change available when the run starts. Whether that exact
/// evidence later authorizes a hard-green or soft-green landing is a separate
/// decision at the landing boundary.
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
    rebase_freshness_from_counts(behind, ahead, force)
}

fn rebase_freshness_from_counts(behind: i64, ahead: i64, force: bool) -> Result<String, String> {
    if behind == 0 {
        return Ok(format!("base: up to date with origin/main (ahead {ahead}, behind 0)"));
    }
    let msg = format!(
        "HEAD is {behind} commit(s) BEHIND origin/main (ahead {ahead}).\n  \
         Validation records exact-commit evidence. Admission requires HEAD to include every \
         origin/main change available when the run starts; hard-green or soft-green landing \
         authorization is decided separately at the landing boundary.\n  \
         Rebase first:  git rebase origin/main\n  \
         To skip only scripts/validate.rs's dirty-working-tree and rebase-freshness checks, pass \
         --skip-inner-dirty-working-tree-and-rebase-freshness-checks. This does not bypass \
         ci-hub validate-lock admission."
    );
    if force {
        Ok(format!("base: STALE, {behind} behind origin/main — forced past the freshness gate"))
    } else {
        Err(msg)
    }
}

fn rebase_freshness_message_bracket() -> Result<(), String> {
    let refusal = rebase_freshness_from_counts(1, 2, false)
        .expect_err("a stale base must still be refused at admission");
    for required in [
        "HEAD is 1 commit(s) BEHIND origin/main (ahead 2)",
        "Validation records exact-commit evidence",
        "hard-green or soft-green landing authorization is decided separately",
        "Rebase first",
    ] {
        if !refusal.contains(required) {
            return Err(format!(
                "rebase freshness: stale-base refusal omitted {required:?}: {refusal}"
            ));
        }
    }
    for false_claim in ["cannot authorize a landing", "will have to be rebuilt"] {
        if refusal.contains(false_claim) {
            return Err(format!(
                "rebase freshness: stale-base refusal retained false landing claim {false_claim:?}"
            ));
        }
    }
    let forced = rebase_freshness_from_counts(1, 2, true)?;
    if !forced.contains("base: STALE, 1 behind origin/main") {
        return Err(format!(
            "rebase freshness: explicit force did not retain its stale-base warning: {forced}"
        ));
    }
    println!(
        "  rebase freshness: stale admission still refuses; exact-commit evidence is separate from hard-green or soft-green landing authorization"
    );
    Ok(())
}

// --------------------------------------------------------------------------- plan

/// What the driver will execute, plus the accounting the ledger needs.
struct Plan {
    cfg: DagConfig,
    /// Canonical serialization of a selection made from the sole committed
    /// validation DAG. The scheduler boundary must still match these bytes;
    /// any validate-side command/dependency/resource rewrite is a refusal.
    committed_selection: Option<String>,
    /// Exact bytes read from the sole committed validation DAG when the
    /// selection was made. The scheduler boundary re-reads this path and
    /// refuses if another path changed the source underneath the selected
    /// in-memory graph.
    committed_source: Option<(PathBuf, Vec<u8>)>,
    /// Second DAG run for a two-part profile. Keeping
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
    /// Tag prefix for the selected committed compatibility population.
    compat_prefix: Option<&'static str>,
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
    /// Nodes withheld because this MACHINE provably cannot run them. Neither a
    /// pass nor a failure: each is reported by name and written to the ledger as
    /// a typed intentional skip whose reason is `host-inapplicable`, which the
    /// parent's separately-reviewed consumer allowlist does not admit, so a run
    /// carrying one does not qualify as landing authority.
    host_inapplicable: Vec<validate_plan::HostInapplicableNode>,
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
    /// Exact selected population for a targeted schema-7 evidence row. This is
    /// deliberately separate from `suite_complete`: it may satisfy one open
    /// cell obligation but can never authorize a whole-run landing receipt.
    cell_evidence_expected: Option<Vec<serde_json::Value>>,
}

struct EnvelopePlan {
    reps: i64,
    baseline: Option<PathBuf>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            cfg: DagConfig::default(),
            committed_selection: None,
            committed_source: None,
            second: None,
            profile: String::new(),
            selection_mode: "full",
            planned_test_nodes: BTreeSet::new(),
            compat: None,
            compat_prefix: None,
            suite_complete: false,
            super_mode: false,
            envelope: None,
            nonblocking: BTreeSet::new(),
            force_keep_going: false,
            host_inapplicable: Vec::new(),
            cacheable: true,
            cell_evidence_expected: None,
        }
    }
}

fn require_committed_scheduler_input(plan: &Plan) -> Result<(), String> {
    let Some(expected) = plan.committed_selection.as_deref() else {
        if plan.committed_source.is_some() {
            return Err("committed DAG source was recorded without a selected scheduler input".into());
        }
        return Ok(());
    };
    let Some((source_path, source_bytes)) = plan.committed_source.as_ref() else {
        return Err("selected scheduler input has no committed DAG source bytes".into());
    };
    if plan.second.is_some() {
        return Err("a committed selection unexpectedly became multiple scheduler DAGs".into());
    }
    let current_source = std::fs::read(source_path).map_err(|error| {
        format!(
            "cannot re-read committed validation DAG {} at the scheduler boundary: {error}",
            source_path.display()
        )
    })?;
    if current_source != *source_bytes {
        return Err(format!(
            "committed validation DAG {} changed after selection; validate refuses to run against a moving source",
            source_path.display()
        ));
    }
    let actual = dag_to_json(&plan.cfg);
    if actual != expected {
        return Err(
            "the selected ci/dag/validate.json graph changed after selection; validate refuses to run remixed nodes"
                .into(),
        );
    }
    Ok(())
}

const RUST_SCRIPT_PRODUCER_TAG: &str = "build.rust_scripts";
const RUST_SCRIPT_COMMAND_PREFIX: &str = "export PATH=\"$PWD/ci/rust-script-bin:$PATH\"; \
    export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT=\"$PWD/target/ci/rust-scripts\"; \
    export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ";

fn committed_rust_script_producer(root: &Path) -> Result<Step, String> {
    let cfg = validate_plan::validation_config(root)?;
    let producers = cfg
        .steps
        .iter()
        .filter(|step| step.tag() == RUST_SCRIPT_PRODUCER_TAG)
        .collect::<Vec<_>>();
    if producers.len() != 1 {
        return Err(format!(
            "committed validation DAG contains {} {RUST_SCRIPT_PRODUCER_TAG} definitions; expected exactly one",
            producers.len()
        ));
    }
    Ok(producers[0].clone())
}

fn serialized_step(step: &Step) -> String {
    let mut cfg = DagConfig::default();
    cfg.steps.push(step.clone());
    dag_to_json(&cfg)
}

fn assert_exact_rust_script_producer(
    actual: &Step,
    committed: &Step,
    context: &str,
) -> Result<(), String> {
    if serialized_step(actual) != serialized_step(committed) {
        return Err(format!(
            "{context} changed the committed {RUST_SCRIPT_PRODUCER_TAG} definition"
        ));
    }
    Ok(())
}

fn rust_script_producer_step() -> Step {
    let mut step = step_with_caps(
        "build",
        "rust_scripts",
        "Build every tracked rust-script before graph consumers run",
        "./ci/prepare-rust-scripts.sh".into(),
        Vec::new(),
        300,
        7200,
        2 * 1024 * 1024 * 1024,
    );
    step.description = "Discovers every tracked rust-script entrypoint, generates one Cargo workspace, runs the existing clippy, release-build, and test-harness phases over that workspace, and publishes the executables. Cargo schedules packages and shared dependencies internally while the producer remains the only writer. It runs after checkout and pin verification; every compiling consumer resolves rust-script through the read-only manifest, so compilation cost cannot migrate according to scheduler order.".into();
    step.hint.classification = dagrun::model::StepClass::CpuBound;
    step.hint.preferred_inner_jobs = Some(8);
    step.jobs_flag = Some(String::new());
    step.jobs_env = Some("CARGO_BUILD_JOBS".into());
    step
}

/// Put rust-script compilation at one explicit point in each scheduled DAG.
///
/// The checked-in lane files carry this node for direct `ci/run-dag.sh` users.
/// Synthetic validation profiles do not, so the driver adds the same node when
/// absent. The producer follows checkout verification and precedes the manifest
/// build; plans with no preflight put it before every prior root. A source
/// invocation later in the graph still reaches its ordinary shebang, but PATH
/// resolves rust-script to the repository shim, which executes the immutable
/// published binary rather than entering Cargo again. Hosted shards explicitly
/// omit predecessors supplied by earlier jobs, so those plans require and check
/// the transported producer output instead of synthesizing another writer.
fn configure_prebuilt_rust_scripts(
    root: &Path,
    plan: &mut Plan,
    require_external_output: bool,
) -> Result<(), String> {
    let committed_producer = committed_rust_script_producer(root)?;
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        if cfg.steps.is_empty() {
            continue;
        }
        let producer_tags: Vec<String> = cfg
            .steps
            .iter()
            .filter(|step| step.tag() == RUST_SCRIPT_PRODUCER_TAG)
            .map(Step::tag)
            .collect();
        if producer_tags.len() == 1
            && cfg
                .steps
                .iter()
                .all(|step| step.cmd.starts_with(RUST_SCRIPT_COMMAND_PREFIX))
        {
            let producer = cfg
                .steps
                .iter()
                .find(|step| step.tag() == RUST_SCRIPT_PRODUCER_TAG)
                .expect("counted exactly once");
            assert_exact_rust_script_producer(
                producer,
                &committed_producer,
                "prebuilt rust-script plan",
            )?;
            if !producer.cmd.ends_with("./ci/prepare-rust-scripts.sh") {
                return Err(format!(
                    "committed rust-script producer command drifted: {}",
                    producer.cmd
                ));
            }
            continue;
        }
        let producer_tag = match producer_tags.as_slice() {
            [] => {
                if require_external_output {
                    String::new()
                } else {
                    cfg.steps.push(committed_producer.clone());
                    RUST_SCRIPT_PRODUCER_TAG.to_string()
                }
            }
            [tag] => {
                let producer = cfg
                    .steps
                    .iter()
                    .find(|step| step.tag() == *tag)
                    .expect("counted exactly once");
                assert_exact_rust_script_producer(
                    producer,
                    &committed_producer,
                    "prebuilt rust-script plan",
                )?;
                if producer.cmd != "./ci/prepare-rust-scripts.sh"
                    && !producer.cmd.ends_with("./ci/prepare-rust-scripts.sh")
                {
                    return Err(format!(
                        "rust-script producer command drifted: {}",
                        producer.cmd
                    ));
                }
                tag.clone()
            }
            tags => {
                return Err(format!(
                    "execution plan contains {} rust-script producer nodes ({}); exactly one may publish the script binaries",
                    tags.len(),
                    tags.join(", ")
                ))
            }
        };
        let has_pin = cfg.steps.iter().any(|step| step.tag() == PIN_GATE_TAG);
        let producer_prerequisite = has_pin.then(|| PIN_GATE_TAG.to_string());
        let prior_roots: BTreeSet<String> = cfg
            .steps
            .iter()
            .filter(|step| step.tag() != producer_tag && step.deps.is_empty())
            .map(Step::tag)
            .collect();
        for step in &mut cfg.steps {
            let tag = step.tag();
            if !producer_tag.is_empty() && tag == producer_tag {
                step.deps = producer_prerequisite.iter().cloned().collect();
            } else if tag != "pre.submodules" {
                if !producer_tag.is_empty() && has_pin {
                    for dependency in &mut step.deps {
                        if dependency == PIN_GATE_TAG {
                            *dependency = producer_tag.clone();
                        }
                    }
                } else if !producer_tag.is_empty() && prior_roots.contains(&tag) {
                    step.deps.push(producer_tag.clone());
                }
                step.deps.sort();
                step.deps.dedup();
            }
            if !step.cmd.starts_with(RUST_SCRIPT_COMMAND_PREFIX) {
                step.cmd = format!("{RUST_SCRIPT_COMMAND_PREFIX}{}", step.cmd);
            }
            if producer_tag.is_empty() {
                step.cmd = format!("./ci/prepare-rust-scripts.sh --check; {}", step.cmd);
            }
        }
        if let Some(stuck) = first_dependency_cycle(&cfg.steps) {
            return Err(format!(
                "rust-script producer ordering created a dependency cycle among: {}",
                stuck.join(", ")
            ));
        }
    }
    Ok(())
}

fn prebuilt_rust_script_plan_bracket(root: &Path) -> Result<String, String> {
    let step = |job: &str, deps: Vec<String>| {
        step_with_caps("fixture", job, "fixture", "true".into(), deps, 30, 30, 1024 * 1024)
    };
    let mut plan = Plan {
        cfg: validate_plan::config_from(
            vec![
                step("root", vec![]),
                step("child", vec!["fixture.root".into()]),
            ],
            "rust-script producer bracket",
        ),
        ..Default::default()
    };
    configure_prebuilt_rust_scripts(root, &mut plan, false)?;
    let producer = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == RUST_SCRIPT_PRODUCER_TAG)
        .ok_or("rust-script producer bracket did not add the producer")?;
    let root_step = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "fixture.root")
        .ok_or("rust-script producer bracket lost the root fixture")?;
    let child = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "fixture.child")
        .ok_or("rust-script producer bracket lost the child fixture")?;
    if producer.cmd != format!("{RUST_SCRIPT_COMMAND_PREFIX}./ci/prepare-rust-scripts.sh")
        || root_step.deps != [RUST_SCRIPT_PRODUCER_TAG.to_string()]
        || child.deps != ["fixture.root".to_string()]
        || !root_step.cmd.starts_with(RUST_SCRIPT_COMMAND_PREFIX)
        || !child.cmd.starts_with(RUST_SCRIPT_COMMAND_PREFIX)
    {
        return Err(format!(
            "rust-script producer bracket lost its single-producer ordering or command wrapper: producer={producer:?} root={root_step:?} child={child:?}"
        ));
    }
    let mut duplicated = Plan {
        cfg: validate_plan::config_from(
            vec![rust_script_producer_step(), rust_script_producer_step()],
            "duplicate rust-script producer bracket",
        ),
        ..Default::default()
    };
    if configure_prebuilt_rust_scripts(root, &mut duplicated, false).is_ok() {
        return Err("rust-script producer bracket accepted duplicate writers".into());
    }
    let mut transported = Plan {
        cfg: validate_plan::config_from(
            vec![step("transported", vec![])],
            "transported rust-script bracket",
        ),
        ..Default::default()
    };
    configure_prebuilt_rust_scripts(root, &mut transported, true)?;
    if transported
        .cfg
        .steps
        .iter()
        .any(|step| step.job == "rust_scripts")
        || !transported.cfg.steps[0]
            .cmd
            .starts_with("./ci/prepare-rust-scripts.sh --check; ")
    {
        return Err(format!(
            "rust-script transported-output bracket synthesized a writer or omitted its check: {:?}",
            transported.cfg.steps
        ));
    }
    let mut preflight = Plan {
        cfg: validate_plan::config_from(
            validate_plan::preflight_nodes(root)?,
            "rust-script preflight bracket",
        ),
        ..Default::default()
    };
    configure_prebuilt_rust_scripts(root, &mut preflight, false)?;
    let producer = preflight
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == RUST_SCRIPT_PRODUCER_TAG)
        .ok_or("rust-script preflight bracket lost the producer")?;
    let manifest_plan = preflight
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
        .ok_or("rust-script preflight bracket lost the manifest producer")?;
    if producer.deps != [PIN_GATE_TAG.to_string()]
        || manifest_plan.deps != [RUST_SCRIPT_PRODUCER_TAG.to_string()]
    {
        return Err(format!(
            "rust-script preflight bracket did not place compilation between pin verification and manifest compilation: producer={producer:?} manifest={manifest_plan:?}"
        ));
    }
    Ok("rust-script build: one producer follows checkout verification and precedes graph consumers; prepared binaries are read-only and duplicate producers refuse".into())
}

fn require_host_capabilities(root: &Path, plan: &Plan) -> Result<(), String> {
    let requirements = validate_plan::host_capability_requirements(root)?;
    let mut needed = BTreeMap::<validate_plan::HostCapability, Vec<String>>::new();
    for step in &plan.cfg.steps {
        if let Some(capability) = requirements.get(&step.tag()) {
            needed.entry(*capability).or_default().push(step.tag());
        }
    }
    let mut absent = Vec::new();
    for (capability, mut steps) in needed {
        steps.sort();
        let verdict = validate_plan::probe_host_capability(capability);
        println!(
            "Host capability {}: {} — {}",
            capability.value(),
            if verdict.present { "PRESENT" } else { "ABSENT" },
            verdict.evidence
        );
        if !verdict.present {
            absent.push(format!(
                "{} required by {}: {}",
                capability.value(),
                steps.join(", "),
                verdict.evidence
            ));
        }
    }
    if absent.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "requested committed profile requires unavailable host capability/capabilities; no nodes were removed: {}",
            absent.join("; ")
        ))
    }
}

/// Withhold every planned node this MACHINE provably cannot run, and say so.
///
/// Applied to the exact maintenance-time plan after its committed dependencies
/// and result consumers have been assembled — so it matches the exact tags the runner
/// will see. Withholding is the only effect: nothing here can turn a node's
/// FAILURE into anything else, because a node that is not withheld runs and is
/// judged exactly as before.
///
/// An unknown capability name, or a retained node depending on a withheld one,
/// is an error that REFUSES the run. Substituting a different node set under the
/// requested profile name would be worse than refusing.
fn withhold_host_inapplicable(root: &Path, plan: &mut Plan) -> Result<(), String> {
    let requirements = validate_plan::host_capability_requirements(root)?;
    if requirements.is_empty() {
        return Ok(());
    }
    // Probe only what this plan actually needs, once per capability.
    let mut needed: BTreeSet<validate_plan::HostCapability> = BTreeSet::new();
    for cfg in std::iter::once(&plan.cfg).chain(plan.second.iter()) {
        for step in &cfg.steps {
            if let Some(capability) = requirements.get(&step.tag()) {
                needed.insert(*capability);
            }
        }
    }
    let mut absent: BTreeMap<validate_plan::HostCapability, String> = BTreeMap::new();
    for capability in needed {
        let verdict = validate_plan::probe_host_capability(capability);
        // Print PRESENT verdicts too: a reader must be able to see that the
        // question was asked and how it was answered, not just its consequences.
        println!(
            "Host capability {}: {} — {}",
            capability.value(),
            if verdict.present { "PRESENT" } else { "ABSENT" },
            verdict.evidence
        );
        if !verdict.present {
            absent.insert(capability, verdict.evidence);
        }
    }
    if absent.is_empty() {
        return Ok(());
    }
    let mut withheld = Vec::new();
    let mut apply = |cfg: &mut DagConfig| -> Result<(), String> {
        let steps = std::mem::take(&mut cfg.steps);
        let (keep, gone) =
            validate_plan::partition_host_inapplicable(steps, &requirements, &absent)?;
        cfg.steps = keep;
        withheld.extend(gone);
        Ok(())
    };
    apply(&mut plan.cfg)?;
    if let Some(second) = plan.second.as_mut() {
        apply(second)?;
    }
    plan.host_inapplicable = withheld;
    // A node can also lose its whole reason to exist WITHOUT declaring anything,
    // when every manifest cell it would run is withheld. That case is computed
    // from the live cell population, never declared; see
    // [`withhold_vacuous_manifest_nodes`].
    withhold_vacuous_manifest_nodes(root, plan, &absent)?;
    for node in &plan.host_inapplicable {
        println!(
            "HOST-INAPPLICABLE: {} will NOT RUN — this machine lacks {} ({}). This is NOT a pass \
             and carries NO coverage for what that node verifies; it is recorded in the ledger as \
             an intentional skip with reason '{}'.",
            node.tag,
            node.capability.value(),
            node.evidence,
            validate_plan::HOST_INAPPLICABLE_REASON
        );
    }
    Ok(())
}

// ------------------------------------------- a node whose whole bucket is gone
//
// hermit#2212 withholds a node that DECLARES a capability this machine lacks.
// hermit#2214 withholds a manifest CELL whose own `requires` declaration names
// one. Between them sits the case neither covers: a DAG node that declares
// nothing itself, but whose entire cell population is withheld at cell level, so
// it would spawn and have nothing at all to run. `target/debug/test-harness` refuses
// that with its vacuity guard — correctly, because `0/0` is not a passing
// population — which leaves the run incomplete rather than recorded.
//
// This is the third case, and it is deliberately the NARROWEST of the three.
//
// WHY IT CANNOT GENERALIZE INTO "SKIP ANYTHING INCONVENIENT":
//
//  1. It is COMPUTED, NEVER DECLARED. There is no list of withholdable nodes
//     anywhere; a node is withheld only when the live cell population it would
//     run is non-empty and every one of those cells is withheld. Adding ONE
//     runnable cell to the bucket un-withholds the node on the next run with no
//     code change, which is exactly the silent-swallow failure a hard-coded node
//     list would rot into.
//  2. IT ADDS NO NEW REASON TO WITHHOLD ANYTHING. Every input is already
//     established: the cell-level withholding of hermit#2214 (closed `requires`
//     vocabulary, one probeable token) and the probe of hermit#2212 (two
//     corroborating sources, absence only). This layer computes a conjunction
//     over decisions already made; it cannot withhold a cell that would
//     otherwise have run, so it cannot enlarge what is omitted by even one cell.
//  3. IT STILL NEVER READS THE NODE. Its inputs are the node's own command line,
//     the manifests, and that probe. No exit code, stderr, timeout or panic can
//     reach it, and the decision is taken before anything spawns.
//  4. AN EMPTY BUCKET IS NOT THIS CASE. `selected == 0` is the pre-existing
//     `empty-manifest-bucket` condition and is explicitly excluded, so this
//     mechanism can never absorb a bucket that simply has no cells.
//  5. IT FAILS CLOSED TOWARD RUNNING at every step. A command it cannot fully
//     model, a bucket missing from the audited required plan, malformed plan
//     metadata, or an uncertain capability probe all leave the node
//     RUNNING — where the harness's own vacuity guard still refuses a vacuous
//     pass.
//  6. THE DENOMINATOR STILL GOES UP. A node withheld here goes into
//     `plan.host_inapplicable` exactly like a declared one: added back into
//     `gates_expected`, named in the plan header, the cost table, the verdict
//     detail and the ledger row, and never written into `gates[]`.

/// One manifest bucket's cell accounting, exactly as `target/debug/test-harness`
/// counts it for the run that bucket's node would perform.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BucketCells {
    lane: String,
    category: String,
    /// Cells the bucket's node would select: one per (test, mode, backend).
    selected: usize,
    /// How many of those the harness would withhold as host-inapplicable.
    withheld: usize,
    /// Which capabilities did the withholding, sorted and deduplicated.
    capabilities: Vec<String>,
}

/// Would this bucket's node have NOTHING to run?
///
/// PURE, and the whole decision. `selected > 0` is load-bearing twice over: a
/// bucket with no cells at all is the pre-existing `empty-manifest-bucket`
/// condition and must not be absorbed here, and `0 == 0` would otherwise make
/// every empty bucket read as host-inapplicable — the exact vacuous accounting
/// this line of work exists to refuse.
///
/// `withheld == selected` rather than `withheld > 0`: one withheld cell in a
/// bucket that still has runnable cells leaves the node running, with the
/// withheld cell recorded by the harness. That is what makes adding a runnable
/// cell back un-withhold the node automatically.
fn bucket_runs_nothing(bucket: &BucketCells) -> bool {
    bucket.selected > 0 && bucket.withheld == bucket.selected
}

/// The `(lane, category)` a manifest bucket node declares, checked against the
/// command that actually selects the cells.
///
/// The typed `manifest` value is the authority. `None` when that value is absent,
/// when the command disagrees with it, when either output path is missing or
/// duplicated, or when the command carries any token this function does not model.
/// THE WHITELIST IS THE POINT: `--results` and `--junit` are accepted only as one
/// value-bearing pair because they change storage, never selection. An unmodelled
/// or selection-affecting `--mode`, `--backend`, `--test`, `--include-occasional`,
/// or anything else means the cell set cannot be proven equal to the bucket
/// accounting, so the node is not a candidate and simply runs.
///
/// `--ci-only` is REQUIRED because the accounting is queried with `--ci-only`;
/// a node selecting a wider population must not be matched against a narrower
/// count.
fn manifest_bucket_of(step: &Step) -> Option<(String, String)> {
    let manifest = step.manifest.as_ref()?;
    let DagManifest {
        lane: declared_lane,
        category: declared_category,
        ..
    } = manifest;
    let tail = step.cmd.split_once("target/debug/test-harness run ")?.1;
    let tokens: Vec<&str> = tail.split_whitespace().collect();
    let mut lane: Option<String> = None;
    let mut category: Option<String> = None;
    let mut results = false;
    let mut junit = false;
    let mut ci_only = false;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--lane" => {
                if lane.is_some() {
                    return None;
                }
                lane = Some((*tokens.get(i + 1)?).to_string());
                i += 2;
            }
            "--category" => {
                if category.is_some() {
                    return None;
                }
                category = Some((*tokens.get(i + 1)?).to_string());
                i += 2;
            }
            "--results" => {
                if results || tokens.get(i + 1)?.starts_with("--") {
                    return None;
                }
                results = true;
                i += 2;
            }
            "--junit" => {
                if junit || tokens.get(i + 1)?.starts_with("--") {
                    return None;
                }
                junit = true;
                i += 2;
            }
            "--ci-only" => {
                if ci_only {
                    return None;
                }
                ci_only = true;
                i += 1;
            }
            // Tokens that change nothing about WHICH cells are selected.
            "--allow-empty" | "--prebuilt" => i += 1,
            // Anything else: unmodelled, so unproven, so not a candidate.
            _ => return None,
        }
    }
    if !ci_only || !results || !junit {
        return None;
    }
    if lane.as_deref() != Some(declared_lane) || category.as_deref() != Some(declared_category) {
        return None;
    }
    Some((declared_lane.clone(), declared_category.clone()))
}


/// Read the exact checked-in required cell population and aggregate it by
/// manifest bucket for the already-resolved absent capabilities.
///
/// `test-harness audit-ci` regenerates this shape from the live YAML manifests
/// and compares it by normalized rows. Keeping the capability on the required
/// cell row means plan construction does not compile or run a second validation
/// driver before dagrun starts. A stale file still fails the mandatory manifest
/// gate, so it can never qualify a receipt.
fn read_bucket_cells(
    root: &Path,
    absent: &BTreeMap<validate_plan::HostCapability, String>,
) -> Result<Vec<BucketCells>, String> {
    let path = root.join("ci/expected-e2e-plan.json");
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    let cells = document
        .get("cells")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{} has no cells array", path.display()))?;
    let mut buckets: BTreeMap<(String, String), BucketCells> = BTreeMap::new();
    for cell in cells {
        let lane = cell
            .get("lane")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{} contains a cell without a lane", path.display()))?;
        let category = cell
            .get("category")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{} contains a cell without a category", path.display()))?;
        let bucket = buckets
            .entry((lane.to_string(), category.to_string()))
            .or_insert_with(|| BucketCells {
                lane: lane.to_string(),
                category: category.to_string(),
                selected: 0,
                withheld: 0,
                capabilities: Vec::new(),
            });
        bucket.selected += 1;
        let mut cell_absent = BTreeSet::new();
        if let Some(values) = cell.get("requires_host_capabilities") {
            let values = values.as_array().ok_or_else(|| {
                format!(
                    "{} contains a non-array requires_host_capabilities field",
                    path.display()
                )
            })?;
            for value in values {
                let name = value.as_str().ok_or_else(|| {
                    format!("{} contains a non-string host capability", path.display())
                })?;
                let capability = validate_plan::HostCapability::from_value(name).ok_or_else(|| {
                    format!(
                        "{} contains unknown host capability {name:?}",
                        path.display()
                    )
                })?;
                if absent.contains_key(&capability) {
                    cell_absent.insert(name.to_string());
                }
            }
        }
        if !cell_absent.is_empty() {
            bucket.withheld += 1;
            bucket.capabilities.extend(cell_absent);
        }
    }
    let mut out = buckets.into_values().collect::<Vec<_>>();
    for bucket in &mut out {
        bucket.capabilities.sort();
        bucket.capabilities.dedup();
    }
    Ok(out)
}

/// Withhold every planned manifest bucket node whose entire cell population is
/// withheld, and say so.
///
/// See the section comment above for why this cannot generalize. Called only
/// when at least one capability is ABSENT, so a machine that has everything
/// pays nothing and emits byte-identical output.
fn withhold_vacuous_manifest_nodes(
    root: &Path,
    plan: &mut Plan,
    absent: &BTreeMap<validate_plan::HostCapability, String>,
) -> Result<(), String> {
    let mut candidates: Vec<(String, String, String)> = Vec::new();
    for cfg in std::iter::once(&plan.cfg).chain(plan.second.iter()) {
        for step in &cfg.steps {
            if let Some((lane, category)) = manifest_bucket_of(step) {
                candidates.push((step.tag(), lane, category));
            }
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let buckets = match read_bucket_cells(root, absent) {
        Ok(buckets) => buckets,
        Err(why) => {
            // FAIL CLOSED TOWARD RUNNING. Without the accounting there is no
            // proof that a bucket is empty of runnable cells, and an unproven
            // omission is worse than a node that runs and refuses itself.
            println!(
                "Host-inapplicable bucket accounting UNAVAILABLE ({why}); NO node was withheld \
                 and every planned manifest bucket node will run."
            );
            return Ok(());
        }
    };
    let by_bucket: BTreeMap<(&str, &str), &BucketCells> = buckets
        .iter()
        .map(|b| ((b.lane.as_str(), b.category.as_str()), b))
        .collect();

    let mut withheld: Vec<validate_plan::HostInapplicableNode> = Vec::new();
    for (tag, lane, category) in &candidates {
        // A bucket with no row selected NO cells at all. That is
        // `empty-manifest-bucket`, not host-inapplicable, and is left alone.
        let Some(bucket) = by_bucket.get(&(lane.as_str(), category.as_str())) else {
            continue;
        };
        if !bucket_runs_nothing(bucket) {
            continue;
        }
        // One typed record needs one capability. More than one means a second
        // probeable token was added without extending this record, so REFUSE
        // rather than pick: refusing is never the bar-lowering direction, and
        // this is unreachable while exactly one token has an absence proof.
        if bucket.capabilities.len() != 1 {
            return Err(format!(
                "manifest bucket {lane}/{category} has every cell withheld by {} capabilities \
                 ({}), and one host-inapplicable record names exactly one; extend the record \
                 before adding a second probeable `requires` token",
                bucket.capabilities.len(),
                bucket.capabilities.join(", ")
            ));
        }
        let name = &bucket.capabilities[0];
        let Some(capability) = validate_plan::HostCapability::from_value(name) else {
            return Err(format!(
                "manifest bucket {lane}/{category} was withheld by capability '{name}', which \
                 the driver's closed vocabulary does not know"
            ));
        };
        let evidence = absent.get(&capability).ok_or_else(|| {
            format!(
                "manifest bucket {lane}/{category} named capability '{name}' without an absent \
                 verdict; refusing inconsistent host-capability accounting"
            )
        })?;
        withheld.push(validate_plan::HostInapplicableNode {
            tag: tag.clone(),
            capability,
            evidence: format!(
                "all {} selected cell(s) of manifest bucket {lane}/{category} are \
                 host-inapplicable: {evidence}",
                bucket.selected
            ),
        });
    }
    if withheld.is_empty() {
        return Ok(());
    }
    let gone: BTreeSet<String> = withheld.iter().map(|n| n.tag.clone()).collect();
    let retained: Vec<(String, String, Vec<String>)> = std::iter::once(&plan.cfg)
        .chain(plan.second.iter())
        .flat_map(|cfg| cfg.steps.iter())
        .filter(|s| !gone.contains(&s.tag()))
        .map(|s| (s.tag(), s.cmd.clone(), s.deps.clone()))
        .collect();
    let (droppable, refusals) = classify_withheld_dependents(&retained, &gone);
    if !refusals.is_empty() {
        return Err(format!(
            "refusing to withhold a manifest bucket node that a NON-RESULT-CONSUMING node \
             depends on: {}; a machine incapability must not silently cascade into unrun work",
            refusals.join(", ")
        ));
    }
    let drop_edge: BTreeSet<(String, String)> = droppable.iter().cloned().collect();
    let apply = |cfg: &mut DagConfig| {
        cfg.steps.retain(|s| !gone.contains(&s.tag()));
        for step in cfg.steps.iter_mut() {
            let tag = step.tag();
            step.deps
                .retain(|d| !drop_edge.contains(&(tag.clone(), d.clone())));
        }
    };
    apply(&mut plan.cfg);
    if let Some(second) = plan.second.as_mut() {
        apply(second);
    }
    for (tag, dep) in &droppable {
        println!(
            "HOST-INAPPLICABLE: dropped result-consumer dependency edge {tag} -> {dep} — the \
             consumer still RUNS and judges the incomplete result set itself; it is not skipped."
        );
    }
    plan.host_inapplicable.extend(withheld);
    Ok(())
}

/// What to do about a RETAINED node that depends on a withheld manifest bucket
/// node. PURE, so both directions are bracketed with planted nodes.
///
/// A withheld bucket node produces per-cell results and nothing else, so a
/// dependent is a RESULT CONSUMER. Leaving the edge in place would strand that
/// consumer unrun — the cascade hermit#2212 refuses — so the edge is dropped and
/// the consumer RUNS and judges the incomplete result set for itself. That
/// measures MORE, not less: if it genuinely needed those results it fails and
/// the run is refused, which is the opposite of an excuse.
///
/// Only a result consumer qualifies. Any other dependent would be treating the
/// withheld node as a prerequisite whose removal cannot be justified from here,
/// so it is a REFUSAL, exactly like hermit#2212's declared-node case.
///
/// Returns the `(dependent, dependency)` edges that may be dropped, and the
/// refusals that must abort the run.
fn classify_withheld_dependents(
    retained: &[(String, String, Vec<String>)],
    gone: &BTreeSet<String>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut droppable = Vec::new();
    let mut refusals = Vec::new();
    for (tag, cmd, deps) in retained {
        // The withheld node's ONLY product is per-cell results under
        // `$E2E_RESULT_ROOT`; naming that root is what makes a dependent a
        // consumer of it rather than a consumer of some prerequisite effect.
        let consumes_results = cmd.contains("$E2E_RESULT_ROOT");
        for dep in deps {
            if !gone.contains(dep) {
                continue;
            }
            if consumes_results {
                droppable.push((tag.clone(), dep.clone()));
            } else {
                refusals.push(format!("{tag} depends on {dep}"));
            }
        }
    }
    (droppable, refusals)
}

fn test_nodes_of(cfg: &DagConfig) -> BTreeSet<String> {
    cfg.steps
        .iter()
        .filter(|s| s.group == "test" || s.group.ends_with("-test") || s.group.ends_with(":test"))
        .map(|s| s.tag())
        .collect()
}

const EXPECTED_SHARED_INTEGRATION_TESTS: [(&str, &str, &str); 2] = [
    ("cli", "test.cli", "privileged-test.cli_kvm"),
    ("hermit_modes", "test.hermit_modes", "privileged-test.pmu_buck_chaos_cases"),
];
const SHARED_INTEGRATION_TEST_BUILDER: &str = "privileged-build.privileged_tests";

fn assert_committed_shared_integration_test_consumers(steps: &[Step]) -> Result<(), String> {
    let mut by_binary: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for step in steps {
        for binary in step.integration_test_binaries.iter().flatten() {
            by_binary.entry(binary).or_default().push(step.tag());
        }
    }
    by_binary.retain(|_, consumers| {
        consumers.sort();
        consumers.iter().any(|tag| tag.starts_with("privileged-"))
            && consumers.iter().any(|tag| !tag.starts_with("privileged-"))
    });
    if by_binary.len() != EXPECTED_SHARED_INTEGRATION_TESTS.len()
        || EXPECTED_SHARED_INTEGRATION_TESTS.iter().any(|(binary, portable, privileged)| {
            by_binary.get(binary) != Some(&vec![(*privileged).into(), (*portable).into()])
        })
    {
        return Err(format!(
            "committed shared integration-test consumers changed: expected={EXPECTED_SHARED_INTEGRATION_TESTS:?}, actual={by_binary:?}"
        ));
    }
    Ok(())
}

fn assert_committed_shared_integration_test_serialization(
    steps: &[Step],
    caps: &BTreeMap<String, i64>,
) -> Result<(), String> {
    assert_committed_shared_integration_test_consumers(steps)?;
    let resource_count = caps
        .keys()
        .chain(steps.iter().flat_map(|step| step.hint.resources.keys()))
        .filter(|resource| resource.starts_with("integration_test_binaries."))
        .collect::<BTreeSet<_>>()
        .len();
    if resource_count != EXPECTED_SHARED_INTEGRATION_TESTS.len() {
        return Err(format!("fused shared integration-test resource count changed: {resource_count}"));
    }
    for (binary, portable, privileged) in EXPECTED_SHARED_INTEGRATION_TESTS {
        let resource = format!("integration_test_binaries.{binary}");
        let expected = vec![
            (SHARED_INTEGRATION_TEST_BUILDER.to_string(), 1),
            (privileged.to_string(), 1),
            (portable.to_string(), 1),
        ];
        let mut actual: Vec<(String, i64)> = steps
            .iter()
            .filter_map(|step| step.hint.resources.get(&resource).map(|n| (step.tag(), *n)))
            .collect();
        actual.sort();
        if caps.get(&resource) != Some(&1) || actual != expected {
            return Err(format!("fused shared-test resource {resource} has cap={:?}, demanders={actual:?}; expected cap=1, demanders={expected:?}", caps.get(&resource)));
        }
    }
    Ok(())
}

/// Read the sole committed validation graph and retain its exact source bytes
/// so the scheduler boundary can reject any intervening file mutation.
fn load_committed_validation_dag(root: &Path) -> Result<(DagConfig, PathBuf, Vec<u8>), String> {
    let path = validate_plan::validation_dag_path(root);
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("{} is not UTF-8: {error}", path.display()))?;
    let cfg = dag_from_json(text)
        .map_err(|error| format!("invalid validation DAG {}: {error}", path.display()))?;
    Ok((cfg, path, bytes))
}

fn finish_committed_selection(
    mut plan: Plan,
    source_path: PathBuf,
    source_bytes: Vec<u8>,
) -> Plan {
    plan.committed_selection = Some(dag_to_json(&plan.cfg));
    plan.committed_source = Some((source_path, source_bytes));
    plan
}

fn requested_step_ids(raw: &str, option: &str) -> Result<BTreeSet<String>, String> {
    let tags = raw
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if tags.is_empty() {
        Err(format!("{option} needs at least one group.job tag"))
    } else {
        Ok(tags)
    }
}

fn expand_strict_compat_alias(
    cfg: &DagConfig,
    tags: &mut BTreeSet<String>,
    profile: &str,
) -> Result<(), String> {
    if !tags.remove(STRICT_COMPAT_SELECTION_ALIAS) {
        return Ok(());
    }
    let compat = cfg
        .steps
        .iter()
        .filter(|step| step.tag() == "compatprep.fixtures" || step.group == "compat")
        .map(Step::tag)
        .collect::<Vec<_>>();
    if compat.is_empty() {
        return Err(format!(
            "{STRICT_COMPAT_SELECTION_ALIAS} is not part of the committed {profile} profile"
        ));
    }
    tags.extend(compat);
    Ok(())
}

const PRIVILEGED_PUBLIC_TAGS: [(&str, &str); 12] = [
    ("build.rust_scripts", "build.rust_scripts"),
    ("check.reverie_pin", "pre.reverie_pin"),
    ("build.privileged_tests", "privileged-only-build.privileged_tests"),
    ("cpuid.faulting", "privileged-only-cpuid.faulting"),
    ("pmu.preemption", "privileged-only-pmu.preemption"),
    (
        "test.pmu_buck_chaos_cases",
        "privileged-only-test.pmu_buck_chaos_cases",
    ),
    ("setup.manifest_plan", "setup.manifest_plan"),
    ("e2e.metadata", "gate.manifest"),
    ("build.manifest_guests", "privileged-build.manifest_guests"),
    (
        "e2e.manifest_applications",
        "privileged-only-e2e.manifest_applications",
    ),
    (
        "e2e.manifest_backend_parity_c",
        "privileged-only-e2e.manifest_backend_parity_c",
    ),
    ("test.cli_kvm", "privileged-only-test.cli_kvm"),
];

fn map_privileged_public_tags(tags: &mut BTreeSet<String>, label: &str) {
    let hosted = label == "hosted-privileged";
    if label != "privileged" && !hosted {
        return;
    }
    for (public, committed_local) in PRIVILEGED_PUBLIC_TAGS {
        if !tags.remove(public) {
            continue;
        }
        let committed = if hosted {
            format!("{committed_local}_on_host")
        } else {
            committed_local.to_string()
        };
        if public != committed {
            println!(
                "Selective validation: privileged public node {public} maps to committed node {committed}"
            );
        }
        tags.insert(committed);
    }
}

fn requalification_identity_field<'a>(
    identity: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, String> {
    identity
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("selected requalification cell has no {field}: {identity}"))
}

fn exact_manifest_value(manifest: &DagManifest) -> Result<serde_json::Value, String> {
    if manifest.lane.is_empty() || manifest.category.is_empty() {
        return Err("selected result manifest has an empty lane or category".into());
    }
    let test = manifest
        .test
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("selected result manifest has no exact test")?;
    let mode = manifest
        .mode
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("selected result manifest has no exact mode")?;
    let backend = manifest
        .backend
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or("selected result manifest has no exact backend")?;
    Ok(serde_json::json!({
        "lane": &manifest.lane,
        "category": &manifest.category,
        "test": test,
        "mode": mode,
        "backend": backend,
    }))
}

fn select_from_committed_decision(
    base: &DagConfig,
    total: usize,
    decision: SelectDecision,
) -> Result<DagConfig, String> {
    match decision {
        SelectDecision::Skip => {
            println!(
                "Selective validation: no CI-relevant changes since baseline — nothing beyond \
                 committed preflight runs (0/{total} lane nodes). The ledger's coverage record \
                 will show zero planned test nodes, so this cannot be misread as a full pass."
            );
            let tags = [
                "pre.submodules",
                PIN_GATE_TAG,
                RUST_SCRIPT_PRODUCER_TAG,
                validate_plan::MANIFEST_PLAN_PRODUCER_TAG,
                "gate.manifest",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
            dagrun::select_steps_by_tags(base, &tags, true)
        }
        SelectDecision::Nodes(keep) => {
            let requested = keep.into_iter().collect::<Vec<_>>();
            let selected = dagrun::select_steps_by_tags(base, &requested, false)?;
            println!(
                "Selective validation: selected {} requested portable DAG node(s); committed \
                 dependency closure contains {}/{} node(s):\n  {}",
                requested.len(),
                selected.steps.len(),
                total,
                requested.join(" ")
            );
            Ok(selected)
        }
        SelectDecision::Full(why) => {
            println!("Selective validation: {why} — running the FULL portable label.");
            Ok(base.clone())
        }
    }
}

/// Add only the canonical manifest executable preparation needed by focused
/// commands. Ordinary build prerequisites remain outside --only; every ID and
/// dependency retained here is read from the committed source without rewriting.
fn retain_focused_manifest_producers(cfg: &DagConfig, tags: &mut BTreeSet<String>) {
    let available = cfg.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    let twins = cfg.steps.iter()
        .filter(|step| step.job == "manifest_guests" && tags.contains(&step.tag()))
        .map(|step| format!("{}_in_pinned_root", step.tag()))
        .filter(|twin| available.contains(twin))
        .collect::<Vec<_>>();
    tags.extend(twins);
    let required_producer = |tag: &str| {
        let tag = tag.strip_prefix("quick-super-").unwrap_or(tag);
        let tag = tag.strip_suffix("_on_host").unwrap_or(tag);
        matches!(tag,
            "setup.manifest_plan" | "setup.manifest_plan_in_pinned_root"
                | "build.rust_scripts" | "build.rust_scripts_in_pinned_root"
                | "setup.pinned_root_fetch"
        )
    };
    loop {
        let additions = cfg.steps.iter()
            .filter(|step| tags.contains(&step.tag()))
            .flat_map(|step| step.deps.iter())
            .filter(|dependency| required_producer(dependency) && !tags.contains(*dependency))
            .cloned().collect::<Vec<_>>();
        if additions.is_empty() { break; }
        tags.extend(additions);
    }
}

/// Runtime plan boundary: every executable path is a selection from the one
/// committed `ci/dag/validate.json`. The maintenance generator is separate and
/// never reaches this function.
fn build_plan(root: &Path, args: &Args, _tmp: &Path) -> Result<Plan, String> {
    let (committed, source_path, source_bytes) = load_committed_validation_dag(root)?;

    if let Some(Focused::Only { lane, nodes }) = &args.focused {
        let lane_cfg = dagrun::select_steps_by_labels(&committed, std::slice::from_ref(lane))?;
        let mut tags = requested_step_ids(nodes, "--only")?;
        map_privileged_public_tags(&mut tags, lane);
        expand_strict_compat_alias(&lane_cfg, &mut tags, lane)?;
        let preflight: &[&str] = match (lane.as_str(), args.allow_local_off_the_record_run) {
            ("hosted-privileged", true) => &["pre.reverie_pin_on_host"],
            ("hosted-privileged", false) => &[
                "pre.reverie_pin_on_host", "build.rust_scripts_on_host",
                "setup.manifest_plan_on_host", "gate.manifest_on_host",
            ],
            (_, true) => &["pre.submodules", PIN_GATE_TAG],
            (_, false) => &[
                "pre.submodules", PIN_GATE_TAG, RUST_SCRIPT_PRODUCER_TAG,
                validate_plan::MANIFEST_PLAN_PRODUCER_TAG, "gate.manifest",
            ],
        };
        tags.extend(preflight.iter().map(|tag| {
            if matches!(lane.as_str(), "quick" | "super") && matches!(*tag,
                RUST_SCRIPT_PRODUCER_TAG | validate_plan::MANIFEST_PLAN_PRODUCER_TAG | "gate.manifest"
            ) {
                format!("quick-super-{tag}")
            } else {
                (*tag).to_string()
            }
        }));
        retain_focused_manifest_producers(&lane_cfg, &mut tags);
        let cfg = dagrun::select_steps_by_tags(
            &lane_cfg,
            &tags.into_iter().collect::<Vec<_>>(),
            true,
        )?;
        let compat = (lane == "portable" && cfg.steps.iter().any(|step| step.group == "compat"))
            .then_some(CompatMode::PortableStrict);
        let plan = Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            profile: args.focused.as_ref().expect("matched focused mode").profile(),
            selection_mode: "only",
            compat,
            compat_prefix: compat.map(|_| "compat."),
            cacheable: false,
            ..Default::default()
        };
        return Ok(finish_committed_selection(plan, source_path, source_bytes));
    }

    if let Some(Focused::RequalifyCell { test, mode, backend }) = &args.focused {
        let matches = validate_cell_results::expected_plan(root)?
            .into_iter()
            .filter(|cell| {
                cell["test"] == *test && cell["mode"] == *mode && cell["backend"] == *backend
            })
            .collect::<Vec<_>>();
        let [identity] = matches.as_slice() else {
            return Err(format!(
                "--requalify-cell must name exactly one currently selected cell; found {} for {test}/{mode}/{backend}",
                matches.len()
            ));
        };
        let exact = DagManifest {
            lane: requalification_identity_field(identity, "lane")?.into(),
            category: requalification_identity_field(identity, "category")?.into(),
            test: Some(test.clone()),
            mode: Some(mode.clone()),
            backend: Some(backend.clone()),
        };
        let lane_cfg = dagrun::select_steps_by_labels(
            &committed,
            std::slice::from_ref(&exact.lane),
        )?;
        let owner = dagrun::result_manifest_owner(&lane_cfg.steps, &exact)?;
        let owner_tag = owner.tag();
        let selected_population = owner
            .effective_result_manifests()
            .iter()
            .map(exact_manifest_value)
            .collect::<Result<Vec<_>, _>>()?;
        if !selected_population.contains(identity) {
            return Err(format!(
                "result owner {owner_tag} does not retain the requested exact identity {identity}"
            ));
        }
        let cfg = dagrun::select_steps_by_tags(&lane_cfg, std::slice::from_ref(&owner_tag), false)?;
        let plan = Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            profile: "cell-requalification".into(),
            selection_mode: "targeted",
            cacheable: false,
            cell_evidence_expected: Some(selected_population),
            ..Default::default()
        };
        return Ok(finish_committed_selection(plan, source_path, source_bytes));
    }

    if let Some(Focused::Selective { shallow }) = &args.focused {
        let base = dagrun::select_steps_by_labels(&committed, &["portable".into()])?;
        let commit_exists = |sha: &str| {
            sh("git", &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_some()
                || Command::new("git")
                    .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false)
        };
        let baseline = if *shallow {
            sh("git", &["rev-parse", "--verify", "HEAD~1"])
        } else {
            let rows = validate_history::read_rows(&ledger_path(root));
            let parent = find_parent(root);
            let slot = slot_name(root, parent.as_deref());
            validate_history::selective_baseline(
                &rows,
                args.baseline.as_deref(),
                &slot,
                &commit_exists,
            )
        };
        match &baseline {
            Some(value) => println!(
                "Selective validation: last-known-green baseline = {value}"
            ),
            None => println!(
                "Selective validation: no trustworthy green baseline; running the FULL portable label."
            ),
        }
        let decision = baseline
            .as_deref()
            .map(|value| ask_selector(root, Some(value)))
            .unwrap_or_else(|| SelectDecision::Full("no trustworthy green baseline".into()));
        let total = base.steps.len();
        let cfg = select_from_committed_decision(&base, total, decision)?;
        let compat = cfg
            .steps
            .iter()
            .any(|step| step.group == "compat")
            .then_some(CompatMode::PortableStrict);
        let plan = Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            profile: "selective".into(),
            selection_mode: "selective",
            compat,
            compat_prefix: compat.map(|_| "compat."),
            cacheable: false,
            ..Default::default()
        };
        return Ok(finish_committed_selection(plan, source_path, source_bytes));
    }

    let committed_label = match (&args.focused, args.level) {
        (Some(Focused::PrivilegedOnly), _) => Some("privileged"),
        (Some(Focused::HostedPortable), _) => Some("hosted-portable"),
        (Some(Focused::HostedPrivileged), _) => Some("hosted-privileged"),
        (Some(Focused::StrictCompat), _) => Some("strict-compat-only"),
        (Some(Focused::PortableStrictCompat), _) => Some("portable-strict-compat-only"),
        (Some(Focused::RrCompat), _) => Some("rr-compat-only"),
        (Some(Focused::SabreCompat), _) => Some("sabre-compat-only"),
        (Some(Focused::E9patchCompat), _) => Some("e9patch-compat-only"),
        (Some(Focused::LiteinstCompat), _) => Some("liteinst-compat-only"),
        (Some(Focused::QemuL2), _) => Some("qemu-l2-only"),
        (Some(Focused::Envelope { .. }), _) => Some("envelope-only"),
        (None, Level::Quick) => Some("quick"),
        (None, Level::PortableOnly) => Some("portable"),
        (None, Level::Full) => Some("full"),
        (None, Level::Super) => Some("super"),
        _ => None,
    };
    if let Some(label) = committed_label {
        if label == "super" && validate_super::repetitions() != validate_super::SUPER_REPETITIONS_DEFAULT
        {
            return Err(format!(
                "SUPER_REPETITIONS cannot change the committed super graph; regenerate ci/dag/validate.json to change its {} repetitions",
                validate_super::SUPER_REPETITIONS_DEFAULT
            ));
        }
        if label == "envelope-only"
            && validate_envelope::l4_reps() != validate_envelope::L4_REPS_DEFAULT
        {
            return Err(format!(
                "L4_REPS cannot change the committed envelope graph; regenerate ci/dag/validate.json to change its {} repetitions",
                validate_envelope::L4_REPS_DEFAULT
            ));
        }
        let mut cfg = dagrun::select_steps_by_labels(&committed, &[label.into()])?;
        let mut selection_mode = "label";
        if let Some(selected) = args.selected.as_deref() {
            let mut tags = requested_step_ids(selected, "--selected")?;
            map_privileged_public_tags(&mut tags, label);
            expand_strict_compat_alias(&cfg, &mut tags, label)?;
            cfg = dagrun::select_steps_by_tags(
                &cfg,
                &tags.into_iter().collect::<Vec<_>>(),
                args.ignore_selected_deps,
            )?;
            selection_mode = "selected";
        }
        let (compat, compat_prefix) = match label {
            "portable" | "full" if cfg.steps.iter().any(|step| step.group == "compat") => {
                (Some(CompatMode::PortableStrict), Some("compat."))
            }
            "strict-compat-only" => (Some(CompatMode::Strict), Some("strictcompat.")),
            "portable-strict-compat-only" => {
                (Some(CompatMode::PortableStrict), Some("portablecompat."))
            }
            "rr-compat-only" => (Some(CompatMode::Rr), Some("rrcompat.")),
            "sabre-compat-only" => (Some(CompatMode::Sabre), Some("sabrecompat.")),
            "e9patch-compat-only" => (Some(CompatMode::E9patch), Some("e9patchcompat.")),
            _ => (None, None),
        };
        let nonblocking = if label == "super" {
            cfg.steps
                .iter()
                .filter(|step| {
                    step.tag().starts_with("superstress.kvm_")
                        || step.tag().starts_with("superstress.dbt_")
                })
                .map(Step::tag)
                .collect()
        } else if label == "envelope-only" {
            cfg.steps
                .iter()
                .filter(|step| step.group == "envelope" && step.job != "build")
                .map(Step::tag)
                .collect()
        } else {
            BTreeSet::new()
        };
        let profile = args
            .focused
            .as_ref()
            .map(Focused::profile)
            .unwrap_or_else(|| args.level.name().to_string());
        let plan = Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            second: None,
            profile,
            selection_mode,
            compat,
            compat_prefix,
            suite_complete: label == "full" && args.selected.is_none(),
            super_mode: label == "super",
            envelope: match &args.focused {
                Some(Focused::Envelope { baseline }) => Some(EnvelopePlan {
                    reps: validate_envelope::L4_REPS_DEFAULT,
                    baseline: baseline.clone(),
                }),
                _ => None,
            },
            nonblocking,
            force_keep_going: label == "envelope-only",
            cacheable: args.selected.is_none()
                && !matches!(label, "portable" | "full" | "envelope-only"),
            ..Default::default()
        };
        return Ok(finish_committed_selection(plan, source_path, source_bytes));
    }
    Err(format!(
        "no committed validation selection is defined for level={:?} focused={:?}",
        args.level, args.focused
    ))
}

/// Make every inherited CPU budget explicit before a source plan crosses the
/// DAG document boundary. `dag_to_json` intentionally does not serialize the
/// execution-only default, so leaving zeroes here would turn validate's 7200s
/// fallback into dagrun's 10s undeclared-node forcing function on reload.
fn materialize_source_cpu_timeouts(cfg: &mut DagConfig) -> Result<(), String> {
    if cfg.default_step_cpu_timeout <= 0 {
        return Err(format!(
            "source plan has no positive default CPU timeout (got {})",
            cfg.default_step_cpu_timeout
        ));
    }
    for step in &mut cfg.steps {
        if step.cpu_timeout <= 0 {
            step.cpu_timeout = cfg.default_step_cpu_timeout;
        }
    }
    Ok(())
}

fn generated_focused_compat_partition(
    root: &Path,
    tmp: &Path,
    mode: CompatMode,
    namespace: &str,
    label: &str,
) -> Result<Vec<Step>, String> {
    let run_root = tmp.join(namespace);
    let fixtures = run_root.join("real-compat-fixtures");
    let shell_build = run_root.join("shell-build");
    let nsswitch = run_root.join("nsswitch.conf");
    let paths = validate_corpus::CorpusPaths {
        root_dir: &root.to_string_lossy(),
        real_compat_fixtures: &fixtures.to_string_lossy(),
        validation_tmp_dir: &run_root.to_string_lossy(),
        shell_build_dir: &shell_build.to_string_lossy(),
    };
    let prep_tag = format!("{namespace}prep.fixtures");
    let mut prep = prepare_fixtures_node_dep(&prep_tag, &fixtures, "compatprep.hermit_release");
    prep.group = format!("{namespace}prep");
    prep.labels = vec![label.into()];

    let mut steps = Vec::new();
    if mode == CompatMode::E9patch {
        let mut nss = nsswitch_fixture_node(&nsswitch);
        nss.group = format!("{namespace}prep");
        nss.labels = vec![label.into()];
        nss.deps = vec!["build.runtime_release".into()];
        prep.deps.push(nss.tag());
        steps.push(nss);
    }
    steps.push(prep);
    let mut probes = validate_plan::compat_nodes(
        root,
        mode,
        &root.join("target/release/hermit").to_string_lossy(),
        &nsswitch.to_string_lossy(),
        &paths,
        Some(&prep_tag),
    )?;
    for step in &mut probes {
        step.group = namespace.into();
        step.labels = vec![label.into()];
    }
    steps.extend(probes);
    Ok(steps)
}

/// Rebuild only the mechanically derived partition of the committed DAG.
///
/// Static nodes are authored in the private `validation_dag_static` module.
/// Compatibility rows come from the checked-in corpus, while stress repetitions
/// come from the typed probe definitions below. The maintenance generator combines
/// those independent inputs into the committed `ci/dag/validate.json`.
fn build_generated_validation_plan(root: &Path, tmp: &Path) -> Result<Plan, String> {
    // These nodes exist only to satisfy dependency closure while the typed
    // compat/stress builders emit their generator-owned partitions. They are
    // discarded before the committed DAG is written; authoritative definitions
    // live in hermit-manifest-plan's private static source.
    let anchor_tags = [
        "build.runtime_release",
        "compatprep.hermit_release",
        "gate.manifest",
        "setup.nextest",
        "doc.doctests",
        "doc.rustdoc",
        "lint.clippy",
        "test.detcore_unit",
        "test.hermit_unit",
        "test.regular_crates",
        "test.rr_suite_contract",
        "super.build_release_hermit",
        "super.build_workspace",
    ];
    let mut steps = anchor_tags
        .iter()
        .map(|tag| {
            let (group, job) = tag
                .split_once('.')
                .ok_or_else(|| format!("invalid generator dependency anchor {tag}"))?;
            let mut step = step_with_caps(
                group,
                job,
                "Generator dependency anchor",
                "true".into(),
                Vec::new(),
                1,
                1,
                1,
            );
            step.labels = vec!["generator-dependency-anchor".into()];
            Ok(step)
        })
        .collect::<Result<Vec<_>, String>>()?;

    let portable_root = tmp.join("strict-compat");
    let portable_fixtures = portable_root.join("real-compat-fixtures");
    let portable_shell_build = portable_root.join("shell-build");
    let portable_paths = validate_corpus::CorpusPaths {
        root_dir: &root.to_string_lossy(),
        real_compat_fixtures: &portable_fixtures.to_string_lossy(),
        validation_tmp_dir: &portable_root.to_string_lossy(),
        shell_build_dir: &portable_shell_build.to_string_lossy(),
    };
    let mut portable_prep = prepare_fixtures_node("compatprep.fixtures", &portable_fixtures);
    portable_prep.deps = [
        "build.runtime_release",
        "doc.doctests",
        "doc.rustdoc",
        "lint.clippy",
        "test.detcore_unit",
        "test.hermit_unit",
        "test.regular_crates",
        "test.rr_suite_contract",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    portable_prep.desc = "Functional compatibility fixtures for direct outer-DAG probes".into();
    portable_prep.description = format!(
        "Generated for this validation under {}; the former nested scheduler is not invoked.",
        portable_root.display()
    );
    portable_prep.labels = vec!["full".into(), "portable".into()];
    steps.push(portable_prep);
    let mut portable = validate_plan::compat_nodes(
        root,
        CompatMode::PortableStrict,
        &root.join("target/ci/hermit-strict").to_string_lossy(),
        "",
        &portable_paths,
        Some("compatprep.fixtures"),
    )?;
    for step in &mut portable {
        step.labels = vec!["full".into(), "portable".into()];
    }
    steps.extend(portable);

    for (mode, namespace, label) in [
        (
            CompatMode::PortableStrict,
            "portablecompat",
            "portable-strict-compat-only",
        ),
        (CompatMode::Strict, "strictcompat", "strict-compat-only"),
        (CompatMode::Sabre, "sabrecompat", "sabre-compat-only"),
        (CompatMode::E9patch, "e9patchcompat", "e9patch-compat-only"),
        (CompatMode::Rr, "rrcompat", "rr-compat-only"),
    ] {
        steps.extend(generated_focused_compat_partition(
            root, tmp, mode, namespace, label,
        )?);
    }

    let super_fixtures = tmp.join("super-compat-fixtures");
    let super_shell_build = tmp.join("super-compat-shell-build");
    let super_paths = validate_corpus::CorpusPaths {
        root_dir: &root.to_string_lossy(),
        real_compat_fixtures: &super_fixtures.to_string_lossy(),
        validation_tmp_dir: &tmp.to_string_lossy(),
        shell_build_dir: &super_shell_build.to_string_lossy(),
    };
    let mut super_prep = prepare_fixtures_node_dep(
        "super-compatprep.fixtures",
        &super_fixtures,
        "super.build_release_hermit",
    );
    super_prep.group = "super-compatprep".into();
    super_prep.labels = vec!["super".into()];
    steps.push(super_prep);
    let only = validate_corpus::portable_super_only()
        .keys()
        .map(|label| label.to_string())
        .collect::<BTreeSet<_>>();
    let mut super_compat = validate_plan::compat_nodes_for(
        root,
        CompatMode::PortableStrict,
        &root.join("target/release/hermit").to_string_lossy(),
        "",
        &super_paths,
        Some("super-compatprep.fixtures"),
        Some(&only),
        Some(validate_super::DEFAULT_GATE_TIMEOUT_S),
    )?;
    for step in &mut super_compat {
        step.labels = vec!["super".into()];
    }
    steps.extend(super_compat);

    let mut stress = validate_super::stress_nodes(
        &root.join("target/release/hermit").to_string_lossy(),
        &root.join("target/debug/hermit").to_string_lossy(),
        tmp,
        validate_super::repetitions(),
        "super.build_release_hermit",
        "super.build_workspace",
    );
    for step in &mut stress {
        step.labels = vec!["super".into()];
    }
    steps.extend(stress);

    let cfg = validate_plan::config_from(steps, "generated validation DAG partition");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        profile: "generated-validation-dag".into(),
        selection_mode: "generator",
        cacheable: false,
        ..Default::default()
    })
}

const HOSTED_VARIANT_SUFFIX: &str = "_on_host";

fn local_validation_step_tag(tag: &str) -> String {
    tag.strip_suffix(HOSTED_VARIANT_SUFFIX)
        .unwrap_or(tag)
        .to_string()
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

/// Apply one selector result while preserving the shipped lane as the tag
/// authority. Producer reuse happens once, after unknown-tag validation, so it
/// covers both dependency-closed subsets and fail-safe full-lane fallbacks.
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
                .map(|a| {
                    a.iter()
                        .filter_map(|value| value.as_str())
                        .map(local_validation_step_tag)
                        .collect()
                })
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

/// Return the tags that cannot be topologically ordered, or `None` when the
/// graph is a DAG. Dependencies naming absent nodes are ignored here; the
/// runner reports those separately.
fn first_dependency_cycle(steps: &[Step]) -> Option<Vec<String>> {
    let present: BTreeSet<String> = steps.iter().map(|s| s.tag()).collect();
    let mut pending: BTreeMap<String, BTreeSet<String>> = steps
        .iter()
        .map(|s| {
            let deps = s
                .deps
                .iter()
                .filter(|d| present.contains(*d))
                .cloned()
                .collect();
            (s.tag(), deps)
        })
        .collect();
    loop {
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(tag, _)| tag.clone())
            .collect();
        if ready.is_empty() {
            return (!pending.is_empty()).then(|| pending.keys().cloned().collect());
        }
        for tag in ready {
            pending.remove(&tag);
            for deps in pending.values_mut() {
                deps.remove(&tag);
            }
        }
    }
}

/// Heavy compatibility preparation is the innermost bound in the validation ladder:
///
/// `420 prep < 480 gate clamp < 600 whole run < 660 local scope < 720 node < 900 job`.
///
/// A 3600s preparation allowance inside a 900s job was unreachable by
/// construction. This bound fires while the scheduler can still name the node
/// and flush its profile row.
const COMPAT_DIAGNOSTIC_WALL_S: i64 = 420;

fn build_release_hermit_node(gate: &str, bin: &str) -> dagrun::model::Step {
    let default = bin.ends_with("target/release/hermit");
    let cmd = if default {
        "cargo build --release -p hermit --features third-party-backends".to_string()
    } else {
        // A caller-supplied binary is reused rather than rebuilt, but it must
        // exist: silently proceeding with a missing binary would fail every row
        // for a reason that has nothing to do with compatibility.
        format!("test -x {}", validate_plan::shell_quote(bin))
    };
    let mut step = step_with_caps(
        "compatprep",
        "hermit_release",
        "Release Hermit for compatibility",
        cmd,
        vec![gate.to_string()],
        COMPAT_DIAGNOSTIC_WALL_S,
        COMPAT_DIAGNOSTIC_WALL_S * 2,
        16 * 1024 * 1024 * 1024,
    );
    if default {
        // A fresh validation checkout has no target cache. Leaving this build
        // undeclared makes the runner box Cargo to one core; that measured
        // 420s and timed out before finishing at be4c0905. The full profile's
        // established eight-job release build completed in 80s on the same
        // host. Declare that width here instead of widening any timeout.
        step.hint.classification = dagrun::model::StepClass::CpuBound;
        step.hint.preferred_inner_jobs = Some(8);
    }
    step
}

fn prepare_fixtures_node(_tag: &str, fixtures: &Path) -> dagrun::model::Step {
    prepare_fixtures_node_dep(_tag, fixtures, "compatprep.hermit_release")
}

/// Exercise committed strict-compatibility nodes through the real outer
/// scheduler without running the corpus.
///
/// The production plan is inspected first. The execution half replaces two
/// guest commands and one ordinary Hermit command with a barrier: all three
/// must be admitted concurrently and must observe dagrun's one outer-step
/// identity. A hidden serial/nested scheduler or restored guest-exclusion
/// resource cannot satisfy that barrier.
fn committed_validation_execution_bracket(root: &Path) -> Result<String, String> {
    let fixture = tempfile::Builder::new()
        .prefix("validate-strict-compat-flat-")
        .tempdir()
        .map_err(|error| format!("strict-compat flatten: cannot create fixture: {error}"))?;
    let first = validate_plan::lane_config(root, "portable")?;
    if first.resource_caps.contains_key("hermit_guest")
        || first
            .steps
            .iter()
            .any(|step| step.hint.resources.contains_key("hermit_guest"))
    {
        return Err(
            "strict-compat flatten: constructed portable plan restored hermit_guest exclusion"
                .into(),
        );
    }
    let regular_crates = first
        .steps
        .iter()
        .find(|step| step.tag() == "test.regular_crates")
        .ok_or("strict-compat flatten: constructed plan lost test.regular_crates")?;
    if regular_crates.hint.preferred_inner_jobs != Some(8)
        || regular_crates.jobs_flag.as_deref() != Some("-j")
    {
        return Err(format!(
            "strict-compat flatten: test.regular_crates must couple nextest to its 8-core box: preferred_inner_jobs={:?} jobs_flag={:?}",
            regular_crates.hint.preferred_inner_jobs, regular_crates.jobs_flag
        ));
    }
    let ordinary = first
        .steps
        .iter()
        .find(|step| step.tag() == "test.hermit_modes")
        .cloned()
        .ok_or("strict-compat flatten: coexistence fixture lost test.hermit_modes")?;

    let expected = validate_corpus::STRICT_COMPAT_TOTAL
        - validate_corpus::portable_super_only().len();
    let probes: Vec<&Step> = first
        .steps
        .iter()
        .filter(|step| step.group == "compat")
        .collect();
    let prep = first
        .steps
        .iter()
        .find(|step| step.tag() == "compatprep.fixtures")
        .ok_or("strict-compat flatten: fixture-preparation node is absent")?;
    if probes.len() != expected
        || first
            .steps
            .iter()
            .any(|step| step.tag() == STRICT_COMPAT_SELECTION_ALIAS)
    {
        return Err(format!(
            "strict-compat flatten: wrong committed shape probes={} expected={expected} prep_deps={:?}",
            probes.len(), prep.deps
        ));
    }
    if first.resource_caps.contains_key("hermit_guest")
        || first
            .steps
            .iter()
            .any(|step| step.hint.resources.contains_key("hermit_guest"))
    {
        return Err(
            "strict-compat flatten: expansion injected a hermit_guest cap or demand".into(),
        );
    }
    let required_prefix = " run --strict --verify --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled --mount=type=tmpfs,target=/test --workdir=/test --env TMPDIR=/tmp -- ";
    if probes.iter().any(|probe| !probe.cmd.contains(required_prefix)) {
        return Err(
            "strict-compat flatten: a portable probe lost minimal base environment or /test working-directory isolation"
                .into(),
        );
    }
    let fixture_readme = "$VALIDATE_RUN_STATE/strict-compat/real-compat-fixtures/README.md";
    let readme_labels = probes
        .iter()
        .filter(|probe| probe.cmd.contains("README.md"))
        .map(|probe| probe.job.as_str())
        .collect::<BTreeSet<_>>();
    let expected_readme_labels = BTreeSet::from([
        "b2sum",
        "base32",
        "base64",
        "bzip2",
        "cat",
        "chown",
        "cksum",
        "du",
        "gzip",
        "head",
        "install",
        "ls",
        "md5sum",
        "pr",
        "readlink",
        "realpath",
        "sha1sum",
        "sha224sum",
        "sha256sum",
        "sha384sum",
        "sha512sum",
        "sum",
        "wc",
        "wc-lines",
        "xz",
        "zstd",
    ]);
    if readme_labels != expected_readme_labels
        || probes
            .iter()
            .filter(|probe| probe.cmd.contains("README.md"))
            .any(|probe| !probe.cmd.contains(fixture_readme))
    {
        return Err(format!(
            "strict-compat flatten: README consumers are not bound to the run-owned fixture: labels={readme_labels:?} fixture={fixture_readme}"
        ));
    }
    for (tag, workload) in [("compat.cargo", "cargo"), ("compat.df", "df")] {
        let command = probes
            .iter()
            .find(|probe| probe.tag() == tag)
            .ok_or_else(|| format!("strict-compat flatten: fixture-backed probe {tag} is absent"))?
            .cmd
            .as_str();
        if !command.contains("tests/compat/real_compat_workload.sh")
            || !command.contains(&format!(" {workload} </dev/null"))
        {
            return Err(format!(
                "strict-compat flatten: {tag} does not use its explicit fixture/tool contract: {command}"
            ));
        }
    }
    if first.steps.iter().any(|step| {
        step.cmd.contains("scripts/validate.rs")
            || step.cmd.contains("pressure-test.rs")
            || step.cmd.contains("dagrun run")
            || step.cmd.contains("run_dag_boxed")
    }) {
        return Err(
            "strict-compat flatten: the production expansion still contains a nested scheduler entrypoint"
                .into(),
        );
    }

    let path_cases = [
        (
            "compat.seq",
            "$VALIDATE_RUN_STATE/strict-compat/real-compat-fixtures",
        ),
        (
            "compat.shell-build",
            "$VALIDATE_RUN_STATE/strict-compat/shell-build",
        ),
        ("compat.top", "$VALIDATE_RUN_STATE/strict-compat/top-home"),
    ];
    for (tag, path) in &path_cases {
        let command = first
            .steps
            .iter()
            .find(|step| step.tag() == *tag)
            .ok_or_else(|| format!("strict-compat flatten: path probe {tag} is absent"))?
            .cmd
            .as_str();
        if !command.contains(path) {
            return Err(format!(
                "strict-compat flatten: {tag} does not use its run-owned path {path}: {command}"
            ));
        }
    }

    let barrier = fixture.path().join("barrier");
    std::fs::create_dir_all(&barrier)
        .map_err(|error| format!("strict-compat flatten: cannot create barrier: {error}"))?;
    let mut execution = validate_plan::config_from_base(
        &first,
        Vec::new(),
        "strict compatibility one-scheduler execution bracket",
    );
    // The committed probes now carry direct preflight edges so --only cannot
    // drop their source/gate ordering. Retain those five prerequisites here as
    // inert commands; the same three workload commands still must overlap.
    let fixture_preflight = [
        "pre.submodules", PIN_GATE_TAG, RUST_SCRIPT_PRODUCER_TAG,
        validate_plan::MANIFEST_PLAN_PRODUCER_TAG, "gate.manifest",
    ];
    for tag in fixture_preflight {
        let source = first.steps.iter().find(|step| step.tag() == tag)
            .ok_or_else(|| format!("strict-compat flatten: missing committed preflight {tag}"))?;
        execution.steps.push(step_with_caps(
            &source.group, &source.job, "inert committed preflight", "true".into(),
            source.deps.clone(), 10, 10, 64 * 1024 * 1024,
        ));
    }
    let mut execution_prep = prep.clone();
    execution_prep.deps.clear();
    execution_prep.cmd = "true".into();
    execution_prep.timeout = 10;
    execution_prep.cpu_timeout = 10;
    execution.steps.push(execution_prep);
    let selected: Vec<Step> = probes.into_iter().take(2).cloned().collect();
    if selected.len() != 2 {
        return Err("strict-compat flatten: fewer than two probes exist for execution".into());
    }
    let tags = [selected[0].tag(), selected[1].tag()];
    for (index, mut probe) in selected.into_iter().enumerate() {
        let own = barrier.join(format!("{index}.ready"));
        let peer = barrier.join(format!("{}.ready", 1 - index));
        let active = barrier.join(format!("{index}.active"));
        let ordinary_active = barrier.join("ordinary.active");
        let observed = barrier.join(format!("{index}.observed"));
        probe.cmd = format!(
            "set -eu; test \"${{DAGRUN_OUTER_RUN:-}}\" = {tag}; test -n \"${{DAGRUN_STEP:-}}\"; touch {active} {own}; i=0; while test ! -e {peer} || test ! -e {ordinary_active}; do i=$((i+1)); test \"$i\" -lt 200; sleep 0.01; done; sleep 0.1; rm -f -- {active}; printf '%s\\n' \"$DAGRUN_OUTER_RUN\" > {observed}",
            tag = validate_plan::shell_quote(&tags[index]),
            active = validate_plan::shell_quote(&active.to_string_lossy()),
            own = validate_plan::shell_quote(&own.to_string_lossy()),
            peer = validate_plan::shell_quote(&peer.to_string_lossy()),
            ordinary_active = validate_plan::shell_quote(&ordinary_active.to_string_lossy()),
            observed = validate_plan::shell_quote(&observed.to_string_lossy()),
        );
        probe.timeout = 10;
        probe.cpu_timeout = 10;
        execution.steps.push(probe);
    }
    let mut ordinary = ordinary;
    let ordinary_observed = barrier.join("ordinary.observed");
    ordinary.deps = vec!["compatprep.fixtures".into(), PIN_GATE_TAG.into(), "gate.manifest".into()];
    ordinary.cmd = format!(
        "set -eu; test \"${{DAGRUN_OUTER_RUN:-}}\" = {tag}; test -n \"${{DAGRUN_STEP:-}}\"; touch {active}; i=0; while test ! -e {first} || test ! -e {second}; do i=$((i+1)); test \"$i\" -lt 200; sleep 0.01; done; sleep 0.1; rm -f -- {active}; printf '%s\\n' \"$DAGRUN_OUTER_RUN\" > {observed}; printf '%s\\n' '{{\"schema\":2,\"executed_tests\":0,\"filtered_tests\":0,\"results\":[]}}' > \"$DAGRUN_TEST_COUNTS_PATH\"",
        tag = validate_plan::shell_quote(&ordinary.tag()),
        active = validate_plan::shell_quote(&barrier.join("ordinary.active").to_string_lossy()),
        first = validate_plan::shell_quote(&barrier.join("0.active").to_string_lossy()),
        second = validate_plan::shell_quote(&barrier.join("1.active").to_string_lossy()),
        observed = validate_plan::shell_quote(&ordinary_observed.to_string_lossy()),
    );
    ordinary.timeout = 10;
    ordinary.cpu_timeout = 10;
    execution.steps.push(ordinary);
    let result = run_lane_once(
        &execution,
        3,
        true,
        0,
        None,
        &fixture.path().join("outer.log"),
        None,
        false,
    );
    let mut observed = (0..2)
        .map(|index| std::fs::read_to_string(barrier.join(format!("{index}.observed"))))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("strict-compat flatten: scheduler evidence missing: {error}"))?;
    observed.push(
        std::fs::read_to_string(&ordinary_observed).map_err(|error| {
            format!("strict-compat flatten: ordinary scheduler evidence missing: {error}")
        })?,
    );
    let expected_observed = tags
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("test.hermit_modes"))
        .collect::<BTreeSet<_>>();
    let expected_outcomes = fixture_preflight.into_iter()
        .chain(std::iter::once("compatprep.fixtures"))
        .chain(expected_observed.iter().copied())
        .collect::<BTreeSet<_>>();
    if !result.ok
        || !result.complete
        || result.outcomes.len() != 9
        || result.outcomes.iter().map(|outcome| outcome.tag.as_str()).collect::<BTreeSet<_>>() != expected_outcomes
        || !result.skipped.is_empty()
        || observed
            .iter()
            .map(|value| value.trim())
            .collect::<BTreeSet<_>>()
            != expected_observed
    {
        return Err(format!(
            "strict-compat flatten: one outer scheduler did not execute two probes and one ordinary Hermit node concurrently: ok={} complete={} outcomes={:?} skipped={:?} observed={observed:?}",
            result.ok,
            result.complete,
            result.outcomes.iter().map(|outcome| outcome.tag.as_str()).collect::<Vec<_>>(),
            result.skipped,
        ));
    }

    Ok(format!(
        "portable strict compatibility: {expected} direct outer nodes without hermit_guest exclusion, run-unique fixture/shell/top paths, one scheduler execution"
    ))
}

/// Exercise both engine choices through the public labelled-DAG entrypoint.
/// The private fixture must execute through Rust and refuse through Python
/// before its marker runs, preserving the structured-result requirement.
fn raw_run_dag_engine_bracket(root: &Path) -> Result<String, String> {
    let fixture = tempfile::Builder::new()
        .prefix("validate-run-dag-engine-")
        .tempdir()
        .map_err(|error| format!("raw run-dag engine: cannot create fixture: {error}"))?;
    let fixture_root = fixture.path();
    std::fs::create_dir_all(fixture_root.join("ci/dag"))
        .map_err(|error| format!("raw run-dag engine: cannot create DAG directory: {error}"))?;
    // Exercise the actual public launcher against an inert committed fixture.
    // Its own ROOT_DIR resolves inside this private tree, so the test needs no
    // alternate-DAG override and cannot launch the product validation graph.
    for relative in ["ci/run-dag.sh", "ci/configure-build-jobs.sh"] {
        std::fs::copy(root.join(relative), fixture_root.join(relative))
            .map_err(|error| format!("raw run-dag engine: cannot copy {relative}: {error}"))?;
    }
    std::os::unix::fs::symlink(root.join("agent-utils"), fixture_root.join("agent-utils"))
        .map_err(|error| format!("raw run-dag engine: cannot link pinned runner: {error}"))?;
    let marker = fixture_root.join("structured-step-executed");
    let counts = serde_json::json!({
        "schema": 2,
        "executed_tests": 1,
        "filtered_tests": 0,
        "results": [{"id": "engine-fixture", "result": "pass", "attempts": 1}],
    }).to_string();
    let mut step = step_with_caps(
        "fixture",
        "structured",
        "structured result engine fixture",
        format!(
            "printf '%s\\n' {} > \"$DAGRUN_TEST_COUNTS_PATH\"; touch {}",
            validate_plan::shell_quote(&counts),
            validate_plan::shell_quote(&marker.to_string_lossy()),
        ),
        Vec::new(),
        30,
        30,
        1024 * 1024,
    );
    step.labels = vec!["hosted-portable".into()];
    step.result_manifests = Some(vec![ResultManifest::StructuredTestResults(
        StructuredTestResultsManifest::current("fixture.structured"),
    )]);
    std::fs::write(
        fixture_root.join("ci/dag/validate.json"),
        dag_to_json(&validate_plan::config_from(vec![step], "structured result engine fixture")),
    )
    .map_err(|error| format!("raw run-dag engine: cannot write committed fixture: {error}"))?;
    let launch = |engine: Option<&str>| -> Result<std::process::Output, String> {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after=2s", "60s"])
            .arg(fixture_root.join("ci/run-dag.sh"))
            .args(["portable", "--allow-cgroup-failure", "--allow-unwise-nest-dagruns", "-q"])
            .current_dir(fixture_root)
            .env_remove("DAGRUN_BIN")
            .env_remove("DAGRUN_ENGINE")
            .env_remove("RUN_DAG_FILE_OVERRIDE")
            .env_remove("VALIDATE_RUN_STATE")
            .env_remove("E2E_RESULT_ROOT")
            .env_remove("E2E_BUILD_ROOT");
        if let Some(engine) = engine {
            command.env("DAGRUN_ENGINE", engine);
        }
        command.output().map_err(|error| format!("raw run-dag engine: cannot launch: {error}"))
    };
    let rust = launch(None)?;
    let rust_stderr = String::from_utf8_lossy(&rust.stderr);
    if !rust.status.success() || !rust_stderr.contains("[dagrun] engine=rust") || !marker.exists() {
        return Err(format!(
            "raw run-dag engine: default runner did not execute the structured committed fixture: status={} marker={} stdout={} stderr={rust_stderr}",
            rust.status, marker.exists(), String::from_utf8_lossy(&rust.stdout),
        ));
    }
    std::fs::remove_file(&marker)
        .map_err(|error| format!("raw run-dag engine: cannot reset marker: {error}"))?;
    let python = launch(Some("python"))?;
    let python_stderr = String::from_utf8_lossy(&python.stderr);
    if python.status.success()
        || !python_stderr.contains("[dagrun] engine=python")
        || !python_stderr.contains("REFUSING to run before any node starts")
        || !python_stderr.contains("Python runner does not implement structured test-result capture")
        || marker.exists()
    {
        return Err(format!(
            "raw run-dag engine: Python did not refuse structured results before execution: status={} marker={} stderr={python_stderr}",
            python.status, marker.exists(),
        ));
    }
    Ok("raw run-dag engine: default Rust executed the structured committed fixture; explicit Python refused before its marker could execute".into())
}

/// Exercise the public labelled-DAG entrypoint used by `.github/workflows/ci-dag.yml`.
/// A capture runner proves the workflow passes the committed bytes plus the
/// requested label to dagrun. It executes no workload.
fn raw_run_dag_strict_compat_bracket(root: &Path) -> Result<String, String> {
    let fixture = tempfile::Builder::new()
        .prefix("validate-run-dag-flat-")
        .tempdir()
        .map_err(|error| format!("raw run-dag: cannot create fixture: {error}"))?;
    let captured = fixture.path().join("captured.json");
    let invoked = fixture.path().join("invoked");
    let runner = fixture.path().join("capture-runner");
    std::fs::write(
        &runner,
        "#!/bin/sh\nset -eu\nprintf '%s\\n' invoked >>\"$RUN_DAG_INVOKED\"\ntest \"$1\" = run\ntest \"$2\" = --dag\ncp -- \"$3\" \"$RUN_DAG_CAPTURE\"\ntest \"$4\" = --labels\ntest \"$5\" = \"$RUN_DAG_EXPECTED_LABEL\"\nshift 5\ntest \"$*\" = \"${RUN_DAG_EXPECTED_SUFFIX:-}\"\ntest -n \"$VALIDATE_RUN_STATE\"\ntest -n \"$E2E_RESULT_ROOT\"\ntest -n \"$E2E_BUILD_ROOT\"\n",
    )
    .map_err(|error| format!("raw run-dag: cannot write capture runner: {error}"))?;
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("raw run-dag: cannot chmod capture runner: {error}"))?;

    for (lane, label) in [
        ("portable", "hosted-portable"),
        ("privileged", "hosted-privileged"),
    ] {
        let output = Command::new(root.join("ci/run-dag.sh"))
            .arg(lane)
            .current_dir(root)
            .env("DAGRUN_BIN", &runner)
            .env("RUN_DAG_CAPTURE", &captured)
            .env("RUN_DAG_INVOKED", &invoked)
            .env("RUN_DAG_EXPECTED_LABEL", label)
            .env("RUN_DAG_EXPECTED_SUFFIX", "")
            .env_remove("RUN_DAG_FILE_OVERRIDE")
            .env_remove("VALIDATE_RUN_STATE")
            .env_remove("E2E_RESULT_ROOT")
            .env_remove("E2E_BUILD_ROOT")
            .output()
            .map_err(|error| format!("raw run-dag: cannot launch {lane}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "raw run-dag: {lane} entrypoint failed with {}: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    let allowed = Command::new(root.join("ci/run-dag.sh"))
        .args([
            "portable",
            "-j",
            "2",
            "--show-plan",
            "--cpu-timeout-multiplier=1.5",
        ])
        .current_dir(root)
        .env("DAGRUN_BIN", &runner)
        .env("RUN_DAG_CAPTURE", &captured)
        .env("RUN_DAG_INVOKED", &invoked)
        .env("RUN_DAG_EXPECTED_LABEL", "hosted-portable")
        .env(
            "RUN_DAG_EXPECTED_SUFFIX",
            "-j 2 --show-plan --cpu-timeout-multiplier=1.5",
        )
        .env_remove("VALIDATE_RUN_STATE")
        .env_remove("E2E_RESULT_ROOT")
        .env_remove("E2E_BUILD_ROOT")
        .output()
        .map_err(|error| format!("raw run-dag: cannot launch allowed controls: {error}"))?;
    if !allowed.status.success() {
        return Err(format!(
            "raw run-dag: allowed non-selection controls failed with {}: {}{}",
            allowed.status,
            String::from_utf8_lossy(&allowed.stdout),
            String::from_utf8_lossy(&allowed.stderr)
        ));
    }

    let captured_bytes = std::fs::read(&captured)
        .map_err(|error| format!("raw run-dag: capture runner received no DAG: {error}"))?;
    let committed = std::fs::read(validate_plan::validation_dag_path(root))
        .map_err(|error| format!("raw run-dag: cannot read committed DAG: {error}"))?;
    if captured_bytes != committed {
        return Err("raw run-dag: launcher did not hand dagrun the committed bytes".into());
    }

    let alternate = fixture.path().join("alternate.json");
    std::fs::write(&alternate, b"{\"description\":\"alternate\",\"steps\":[]}")
        .map_err(|error| format!("raw run-dag: cannot write alternate DAG: {error}"))?;
    std::fs::remove_file(&invoked)
        .map_err(|error| format!("raw run-dag: cannot reset invocation marker: {error}"))?;
    let refused = Command::new(root.join("ci/run-dag.sh"))
        .arg("portable")
        .current_dir(root)
        .env("DAGRUN_BIN", &runner)
        .env("RUN_DAG_CAPTURE", &captured)
        .env("RUN_DAG_INVOKED", &invoked)
        .env("RUN_DAG_EXPECTED_LABEL", "hosted-portable")
        .env("RUN_DAG_FILE_OVERRIDE", &alternate)
        .output()
        .map_err(|error| format!("raw run-dag: cannot launch override refusal: {error}"))?;
    let refusal_stderr = String::from_utf8_lossy(&refused.stderr);
    if refused.status.success()
        || !refusal_stderr.contains("RUN_DAG_FILE_OVERRIDE was removed")
        || invoked.exists()
        || std::fs::read(&captured)
            .map_err(|error| format!("raw run-dag: cannot re-read captured DAG: {error}"))?
            != committed
    {
        return Err(format!(
            "raw run-dag: alternate DAG input was not refused without changing the captured committed DAG: status={} stderr={refusal_stderr:?}",
            refused.status
        ));
    }

    for args in [
        vec!["portable".to_owned(), "--dag".to_owned(), alternate.display().to_string()],
        vec!["portable".to_owned(), format!("--dag={}", alternate.display())],
        vec!["portable".to_owned(), "--labels".to_owned(), "full".to_owned()],
        vec!["portable".to_owned(), "--labels=full".to_owned()],
        vec!["portable".to_owned(), "--selected".to_owned(), "pre.submodules".to_owned()],
        vec!["portable".to_owned(), "--selected=pre.submodules".to_owned()],
        vec!["portable".to_owned(), "--ignore-selected-deps".to_owned()],
        vec!["portable".to_owned(), "--args".to_owned(), "echo".to_owned()],
        vec!["portable".to_owned(), "--args=echo".to_owned()],
        vec!["portable".to_owned(), "--stress".to_owned(), "2".to_owned()],
        vec!["portable".to_owned(), "--stress=2".to_owned()],
        vec![
            "portable".to_owned(),
            "--resource-caps-path".to_owned(),
            alternate.display().to_string(),
        ],
        vec![
            "portable".to_owned(),
            format!("--resource-caps-path={}", alternate.display()),
        ],
        vec!["portable".to_owned(), "--small-default-cap".to_owned()],
        vec![
            "portable".to_owned(),
            "--small-default-cap=true".to_owned(),
        ],
    ] {
        let output = Command::new(root.join("ci/run-dag.sh"))
            .args(&args)
            .current_dir(root)
            .env("DAGRUN_BIN", &runner)
            .env("RUN_DAG_CAPTURE", &captured)
            .env("RUN_DAG_INVOKED", &invoked)
            .env("RUN_DAG_EXPECTED_LABEL", "hosted-portable")
            .output()
            .map_err(|error| format!("raw run-dag: cannot launch authority refusal: {error}"))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success()
            || !stderr.contains("refusing caller graph/selection override")
            || invoked.exists()
        {
            return Err(format!(
                "raw run-dag: authority override {args:?} reached the runner or was not refused: status={} stderr={stderr:?}",
                output.status
            ));
        }
    }
    let cfg = dag_from_json(
        std::str::from_utf8(&committed)
            .map_err(|error| format!("raw run-dag: committed DAG is not UTF-8: {error}"))?,
    )
    .map_err(|error| format!("raw run-dag: committed DAG is invalid: {error}"))?;
    let portable = dagrun::select_steps_by_labels(&cfg, &["hosted-portable".into()])?;
    if portable
        .steps
        .iter()
        .any(|step| step.cmd.contains("dagrun run") || step.cmd.contains("scripts/validate.rs"))
    {
        return Err("raw run-dag: portable label contains a nested scheduler command".into());
    }

    Ok(format!(
        "raw run-dag: public launcher passed the {}-node committed superset plus exact hosted portable/privileged labels; non-selection controls forwarded; alternate DAG and label overrides refused before runner invocation; no nested scheduler command",
        cfg.steps.len(),
    ))
}

/// The functional-fixture prep node, with an explicit predecessor.
///
/// The `super` suite already builds a release Hermit under its own tag, so it
/// hangs the fixtures off THAT node instead of adding a second identical build.
fn prepare_fixtures_node_dep(
    _tag: &str,
    fixtures: &Path,
    dep: &str,
) -> dagrun::model::Step {
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
/// host identity-daemon races out of the e9patch compatibility measurement.
fn nsswitch_fixture_node(path: &Path) -> dagrun::model::Step {
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

// `shard_node` used to live here: it wrapped the selected node in a synthetic
// `shard.*` step whose command was `./ci/run-node.sh`, nesting a second
// dagrun under this one. See the `Focused::Only` branch in
// `build_plan` for why that broke `--only` and what replaced it. `ci/run-node.sh`
// itself is UNCHANGED and still serves the hosted GitHub fan-out, which really
// does need a standalone runner per shard job.

fn step_with_caps(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem: i64,
) -> dagrun::model::Step {
    dagrun::model::Step {
        group: group.into(),
        job: job.into(),
        desc: desc.into(),
        description: String::new(),
        cmd,
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        labels: Vec::new(),
        deps,
        env: BTreeMap::new(),
        hint: dagrun::model::ResourceHint {
            rss_baseline_bytes: Some(mem),
            hard_mem_max_bytes: Some(mem),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        // Undeclared, as these nodes were before the runner grew the fields. See
        // validate_plan::node for why this is not `Some(vec![])`.
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    }
}

// --------------------------------------------------------------------------- reporting

/// A completed node uses EX_TEMPFAIL to say that it could not determine its
/// condition. This is deliberately the only nonzero code that is not a product
/// failure; every other nonzero remains loud.
const NO_RESULT_EXIT_CODE: i64 = 75;

fn outcome_is_no_result(outcome: &StepOutcome) -> bool {
    !outcome.aborted && outcome.returncode == Some(NO_RESULT_EXIT_CODE)
}

fn outcome_is_failure(outcome: &StepOutcome) -> bool {
    !outcome.ok && !outcome.aborted && !outcome_is_no_result(outcome)
}

/// Count the failures represented by the final verdict.
///
/// Compatibility rows have their own policy classification, so only their
/// separately counted blocking rows plus failures outside the matrix belong in
/// the total. Every other profile already counts its failed DAG nodes in
/// `blocking_failure_nodes`. In particular, the super stress table groups those
/// same failed repetition nodes by probe for display; adding that grouped count
/// here would count one failure once as a node and again as a probe.
fn effective_failure_count(
    compat: Option<CompatMode>,
    blocking_failure_nodes: usize,
    compat_blocking: usize,
    compat_structural_failures: usize,
) -> usize {
    if compat.is_some() {
        compat_blocking + compat_structural_failures
    } else {
        blocking_failure_nodes
    }
}

/// The blocking-failure headline: the count and the names, from ONE collection.
///
/// ⚠️ THIS EXISTS BECAUSE THE COUNT AND THE LIST DISAGREED IN PRODUCTION. On the
/// owner's run at `4e168f2aa5b9` the verdict read `9 blocking failure(s):` and
/// then named EIGHT — the list was built with a bare `.take(8)` while the count
/// came from `.count()` on the same filter. The dropped node,
/// `test.sabre_examples`, is not an excused cell, so an operator who fixed the
/// eight they were shown would re-run into a red nobody had named.
///
/// Pure, and returns both halves together, so a caller cannot print one without
/// the other and `summary_listing_bracket` can pin every case without a DAG.
fn blocking_listing<'a>(
    outcomes: &'a [StepOutcome],
    nonblocking: &BTreeSet<String>,
    effective_failures: usize,
) -> (Vec<&'a str>, String) {
    let named: Vec<&str> = outcomes
        .iter()
        .filter(|o| outcome_is_failure(o) && !nonblocking.contains(&o.tag))
        .map(|o| o.tag.as_str())
        .collect();
    let unclassified = outcomes.iter()
        .filter(|outcome| !outcome.ok && !nonblocking.contains(&outcome.tag))
        .map(|outcome| outcome.tag.as_str())
        .filter(|tag| !named.contains(tag)).collect();
    format_blocking_listing(named, unclassified, effective_failures)
}

fn classified_blocking_listing<'a>(
    classification: &'a validate_classification::RunClassification,
    nonblocking: &BTreeSet<String>,
    effective_failures: usize,
) -> (Vec<&'a str>, String) {
    let named = classification.product_failure_nodes.iter()
        .filter(|tag| !nonblocking.contains(*tag)).map(String::as_str).collect();
    let unclassified = classification.no_result_nodes.iter()
        .chain(classification.understood_infrastructure_failure_nodes.keys())
        .chain(classification.understood_prerequisite_failure_nodes.iter())
        .filter(|tag| !nonblocking.contains(*tag))
        .map(String::as_str).collect();
    format_blocking_listing(named, unclassified, effective_failures)
}

fn format_blocking_listing<'a>(
    named: Vec<&'a str>,
    unclassified: Vec<&str>,
    effective_failures: usize,
) -> (Vec<&'a str>, String) {
    // A cap is defensible; a SILENT cap is not. Name the remainder as a number.
    const NAMED_CAP: usize = 12;
    let shown = named.len().min(NAMED_CAP);
    let elided = named.len() - shown;
    let mut listing = if named.is_empty() {
        String::new()
    } else {
        format!(
            ": {}{}",
            named[..shown].join(", "),
            if elided == 0 {
                String::new()
            } else {
                format!(" (+{elided} more, see the node table above)")
            }
        )
    };
    // ⚠️ A HEADLINE LARGER THAN ITS OWN LIST MEANS SOMETHING BLOCKING IS
    // UNCOUNTABLE FROM THIS SET, and that is how a timed-out node vanished from
    // this run. `effective_failures` legitimately exceeds `named` for the compat
    // profile, which adds blocking program rows that are not independently
    // listed here — so this does not refuse, it SAYS SO. Silence was the only
    // unacceptable option.
    if effective_failures > named.len() {
        listing.push_str(&format!(
            " ⚠️ {} counted blocking node(s) are NOT NAMEABLE from the failure set; \
             the node table is authoritative",
            effective_failures - named.len()
        ));
    }
    // ⚠️ AND THE OTHER DIRECTION, WHICH IS THE ONE THAT ACTUALLY BIT US: A NODE
    // THAT IS NOT OK AND IS NOT A `FAILURE` EITHER.
    //
    // On `4e168f2aa5b9` TEN nodes printed `✗ FAIL` and the headline said NINE.
    // The tenth, `privileged-e2e.manifest_backend_parity_c`, hit its 120s wall.
    // A budget kill is neither `ok` nor a `failure` by `outcome_is_failure`, and
    // its reason did not match the budget prefixes either, so it was invisible to
    // the failure count AND to `timed_out_nodes` — it had no class at all, and a
    // state with no value for it is a state that does not get reported.
    //
    // ⚠️ IT IS DELIBERATELY NOT FOLDED INTO THE FAILURE COUNT. A timeout is "ran
    // and produced no verdict", and calling it a failure would assert a product
    // claim the run never established — the same distinction, destroyed in the
    // other direction. It gets its OWN count and its OWN names, which is what
    // "not a pass, not a failure, and not nothing" requires.
    if !unclassified.is_empty() {
        listing.push_str(&format!(
            " ⚠️ plus {} node(s) that did NOT pass and produced NO VERDICT (budget kill, \
             abort, or an unclassified exit) and are therefore in NEITHER the count above \
             nor any failure class: {}",
            unclassified.len(),
            unclassified.join(", ")
        ));
    }
    (named, listing)
}

/// Explain why a measured graph cannot support a green verdict.
///
/// Kept in one helper so the terminal summary and the scheduler regression use
/// the same wording for dependency-skipped and otherwise incomplete work.
fn execution_completeness_details(skipped: &[String], execution_complete: bool) -> Vec<String> {
    let mut detail = Vec::new();
    if !skipped.is_empty() {
        detail.push(format!("{} node(s) never ran because a dependency failed", skipped.len()));
    }
    if !execution_complete {
        detail.push(
            "not every required node completed with a non-aborted outcome; dependency-skipped, \
             aborted, timed-out, or unreported work made the run incomplete"
                .into(),
        );
    }
    detail
}

/// Cap a refusal's item list and NAME THE REMAINDER.
///
/// ⚠️ A CAP IS DEFENSIBLE; A SILENT CAP IS NOT. Every caller here prints a count
/// taken from `.len()` and then the list. With more offenders than the cap, the
/// operator is told a number, shown fewer, and told nothing about the
/// difference -- so the refusal understates the very thing it exists to report,
/// and it understates it in the direction of "less work to do".
///
/// `blocking_listing` above fixed this shape at the blocking-failure headline
/// (hermit#2636). The class was three, not one: the two `RunSummary::refused(3, ..)`
/// sites for ungrantable resources and node-vs-whole-run budgets carried the same
/// bare `.take(8)`. This is the shared cap-and-declare those sites use, so the
/// next one cannot be added without going through a function whose whole purpose
/// is to state the remainder.
const REFUSAL_ITEM_CAP: usize = 8;

fn capped_refusal_items(items: Vec<String>) -> Vec<String> {
    let total = items.len();
    let shown = total.min(REFUSAL_ITEM_CAP);
    let elided = total - shown;
    let mut out: Vec<String> = items.into_iter().take(shown).collect();
    if elided != 0 {
        out.push(format!("  (+{elided} more not shown)"));
    }
    out
}

/// Pin the count-versus-enumeration invariant that broke on `4e168f2aa5b9`.
///
/// The regression it exists to catch is a cap that drops names silently. Case 2
/// is the exact production shape: NINE blocking failures, of which the old
/// `.take(8)` named eight and said nothing about the ninth.
fn summary_listing_bracket() -> Result<String, String> {
    let row = |tag: &str, ok: bool| StepOutcome {
        tag: tag.to_string(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        test_results: None,
        returncode: Some(if ok { 0 } else { 1 }),
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: String::new(),
        aborted: false,
    };
    let none: BTreeSet<String> = BTreeSet::new();

    // A failed super stress repetition is already one failed DAG node. The
    // grouped per-probe table may also report one failing probe, but that is a
    // second view of the same failure and must not increase the headline.
    if effective_failure_count(None, 1, 1, 1) != 1 {
        return Err(
            "failure count: one super stress repetition was counted once as a node and again as a probe"
                .into(),
        );
    }
    // Compatibility is deliberately different: its policy owns the matrix
    // rows, while only failures outside that matrix are added.
    if effective_failure_count(Some(CompatMode::Strict), 99, 2, 1) != 3 {
        return Err(
            "failure count: compatibility matrix and structural failure populations were not kept separate"
                .into(),
        );
    }

    // 1. Every failure is named when the set is small.
    let small: Vec<StepOutcome> = (0..3).map(|i| row(&format!("n{i}"), false)).collect();
    let (named, listing) = blocking_listing(&small, &none, 3);
    if named.len() != 3 || !listing.contains("n2") {
        return Err(format!("small set lost a name: {listing}"));
    }

    // 2. THE PRODUCTION CASE. Nine failures must yield nine names, not eight.
    let nine: Vec<StepOutcome> = (0..9).map(|i| row(&format!("f{i}"), false)).collect();
    let (named, listing) = blocking_listing(&nine, &none, 9);
    if named.len() != 9 {
        return Err(format!("nine failures produced {} names", named.len()));
    }
    for i in 0..9 {
        if !listing.contains(&format!("f{i}")) {
            return Err(format!("f{i} was counted but not named: {listing}"));
        }
    }

    // 3. Above the cap the remainder is STATED, never dropped in silence.
    let many: Vec<StepOutcome> = (0..15).map(|i| row(&format!("m{i}"), false)).collect();
    let (named, listing) = blocking_listing(&many, &none, 15);
    if named.len() != 15 || !listing.contains("(+3 more") {
        return Err(format!("cap did not declare its remainder: {listing}"));
    }

    // 4. A headline larger than its own list ANNOUNCES the gap. This is the
    //    timed-out-node shape: counted somewhere, nameable from nothing.
    let (_n, listing) = blocking_listing(&nine, &none, 10);
    if !listing.contains("NOT NAMEABLE") {
        return Err(format!("count exceeding the list was not announced: {listing}"));
    }

    // 5. THE OTHER PRODUCTION SHAPE: ten nodes not ok, nine classified as
    //    failures, one a budget kill. The tenth must be COUNTED AND NAMED in its
    //    own right, and must NOT be silently absorbed into the failure count.
    let mut ten = nine.clone();
    let mut killed = row("privileged-e2e.manifest_backend_parity_c", false);
    killed.aborted = true; // a budget kill: not ok, and not a `failure` either
    ten.push(killed);
    let (named, listing) = blocking_listing(&ten, &none, 9);
    if named.len() != 9 {
        return Err(format!("budget kill was absorbed into the failure count: {}", named.len()));
    }
    if !listing.contains("NO VERDICT") || !listing.contains("manifest_backend_parity_c") {
        return Err(format!("a node that did not pass went unreported: {listing}"));
    }

    // 6. A nonblocking row is excluded from BOTH halves, not just one.
    let excused: BTreeSet<String> = BTreeSet::from(["f0".to_string()]);
    let (named, listing) = blocking_listing(&nine, &excused, 8);
    if named.len() != 8 || listing.contains("f0") {
        return Err(format!("nonblocking row leaked into the list: {listing}"));
    }

    // 7. THE OTHER TWO INSTANCES OF THE SAME SHAPE. `capped_refusal_items` is
    //    what the two `RunSummary::refused(3, ..)` sites use; pin it here so the
    //    class is bracketed in one place rather than at each call site.
    //
    //    ⚠️ CONTROL FIRST, AND IT MUST NOT ELIDE. Without a case that stays
    //    whole, a helper that appended "(+N more)" unconditionally -- or one
    //    that dropped everything -- would satisfy every remaining assertion.
    let exactly_at_cap: Vec<String> =
        (0..REFUSAL_ITEM_CAP).map(|i| format!("  a{i}")).collect();
    let kept = capped_refusal_items(exactly_at_cap);
    if kept.len() != REFUSAL_ITEM_CAP || kept.iter().any(|l| l.contains("more not shown")) {
        return Err(format!(
            "a list exactly at the cap must be shown whole and unannotated: {kept:?}"
        ));
    }

    // 8. One over the cap: the remainder is STATED, and the arithmetic is right.
    let over: Vec<String> = (0..REFUSAL_ITEM_CAP + 1).map(|i| format!("  b{i}")).collect();
    let capped = capped_refusal_items(over);
    if capped.len() != REFUSAL_ITEM_CAP + 1 {
        return Err(format!("cap produced {} lines, want {}", capped.len(), REFUSAL_ITEM_CAP + 1));
    }
    if !capped.last().is_some_and(|l| l.contains("(+1 more not shown)")) {
        return Err(format!("one over the cap did not declare its remainder: {capped:?}"));
    }

    // 9. Well over the cap: the elided count is total-minus-shown, not a guess.
    let far_over: Vec<String> = (0..REFUSAL_ITEM_CAP + 7).map(|i| format!("  c{i}")).collect();
    let capped = capped_refusal_items(far_over);
    if !capped.last().is_some_and(|l| l.contains("(+7 more not shown)")) {
        return Err(format!("elided count is wrong: {capped:?}"));
    }
    //    And nothing above the cap is silently retained OR silently dropped: the
    //    shown items must be the FIRST ones, in order.
    if capped.first().map(String::as_str) != Some("  c0") {
        return Err(format!("cap did not keep the first items in order: {capped:?}"));
    }

    // 10. The empty case says nothing at all -- no "(+0 more)" noise.
    if !capped_refusal_items(Vec::new()).is_empty() {
        return Err("an empty list must produce no lines".to_string());
    }

    Ok("summary listing: count and enumeration agree across 10 cases (the 9-failure \
shape named in full, the cap states its remainder, a budget kill is counted \
and named without being folded into the failure count, and the two refusal \
sites' shared cap is whole at the cap, declares +1 and +7 above it, and is \
silent when empty)"
        .to_string())
}

fn ledger_gate_result(outcome: &StepOutcome) -> &'static str {
    if outcome.ok {
        "pass"
    } else if outcome_is_no_result(outcome) {
        "no_result"
    } else {
        "fail"
    }
}

fn ledger_run_results(
    exit_code: u8,
    failures: usize,
    no_results: usize,
    interrupted: bool,
) -> (&'static str, &'static str) {
    let raw = if exit_code == 0 && failures == 0 && no_results == 0 {
        "pass"
    } else {
        "fail"
    };
    let result = if failures > 0 {
        "fail"
    } else if interrupted || exit_code == NO_RESULT_EXIT_CODE as u8 {
        "no_result"
    } else {
        raw
    };
    (raw, result)
}

fn completed_exit_code(
    effective_failures: usize,
    no_results: usize,
    run_timed_out: bool,
    unexplained_runner_failure: bool,
) -> u8 {
    if effective_failures > 0 || unexplained_runner_failure {
        1
    } else if no_results > 0 || run_timed_out {
        NO_RESULT_EXIT_CODE as u8
    } else {
        0
    }
}

/// Print the super stress table without turning absent product results into
/// product failures. `rates` must be computed from product-result outcomes
/// only; `planned - ran` is therefore the number of selected repetitions that
/// produced no product result and belongs to the run's incomplete accounting.
fn print_super_stress_verdict(
    rates: &[validate_super::ProbeRate],
    reps: i64,
    jobs: i64,
    host_cpus: usize,
) -> usize {
    println!("\n== Super stress pass rates ==");
    println!("Repetitions: {reps}; scheduler width: {jobs}; online CPUs: {host_cpus}");
    let mut blocking = 0usize;
    for rate in rates {
        let slug = rate.probe.slug();
        let product_failures = rate.ran.saturating_sub(rate.passed);
        let without_product_result = rate.planned.saturating_sub(rate.ran);
        if rate.ran == 0 {
            println!(
                "  NO_RESULT {slug:<24} 0/{} selected repetition(s) produced a product result",
                rate.planned
            );
            continue;
        }
        let pct = 100 * rate.passed / rate.ran.max(1);
        if product_failures == 0 && without_product_result == 0 {
            println!("  ✅ {slug:<24} {}/{} (100%)", rate.passed, rate.ran);
        } else if product_failures == 0 {
            println!(
                "  NO_RESULT {slug:<24} {}/{} product result(s) passed ({pct}%); {} of {} \
                 selected repetition(s) did not produce a product result",
                rate.passed,
                rate.ran,
                without_product_result,
                rate.planned,
            );
        } else if rate.probe.nonblocking() {
            println!(
                "  ⚠️  {slug:<24} {}/{} product result(s) passed ({pct}%){} — NONBLOCKING: this \
                 row was dead code in validate.sh (`backend_selector_supported` is undefined, so \
                 the guard was always false) and has never been measured; reporting it, not \
                 ratcheting it.",
                rate.passed,
                rate.ran,
                if without_product_result == 0 {
                    String::new()
                } else {
                    format!(
                        "; {without_product_result} of {} selected repetition(s) did not produce \
                         a product result",
                        rate.planned
                    )
                },
            );
        } else {
            println!(
                "  ⚠️  {slug:<24} {}/{} product result(s) passed ({pct}%){}",
                rate.passed,
                rate.ran,
                if without_product_result == 0 {
                    String::new()
                } else {
                    format!(
                        "; {without_product_result} of {} selected repetition(s) did not produce \
                         a product result",
                        rate.planned
                    )
                },
            );
            blocking += 1;
        }
    }
    blocking
}

/// Per-node cost table, built entirely from typed `StepOutcome` fields.
fn print_cost_table(
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
    skipped: &[String],
    host_inapplicable: &[validate_plan::HostInapplicableNode],
) {
    println!("\n=== per-node cost (dagrun) ===");
    println!("{:<44} {:>9}  {:<8} reason/returncode", "node", "seconds", "status");
    println!("{}", "-".repeat(84));
    let mut total = 0.0_f64;
    for o in outcomes {
        total += o.duration_s;
        let status = match node_classification(o, attempts) {
            NodeClassification::Pass => "ok",
            NodeClassification::ProductFailure => "FAIL",
            NodeClassification::UnderstoodInfrastructureFailure => "INFRA",
            NodeClassification::UnderstoodPrerequisiteFailure => "PREREQ",
            NodeClassification::NoResult => "NO_RESULT",
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
    // Listed AFTER the TOTAL and outside the ok/FAIL/ABORTED column on purpose:
    // a host-inapplicable node has no status in that vocabulary. It did not
    // pass, and printing it in a table of statuses is how it would come to look
    // like one.
    for node in host_inapplicable {
        println!(
            "\nhost-inapplicable (NOT RUN, NOT a pass, no coverage): {} — this machine lacks {} \
             ({})",
            node.tag,
            node.capability.value(),
            node.evidence
        );
    }
}

/// Per-program compatibility summary, built from typed node outcomes rather than
/// a scraped TSV. Reproduces `print_compatibility_summary`'s category table.
fn print_compat_summary(
    mode: CompatMode,
    prefix: &str,
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
) -> (usize, usize, Vec<String>, BTreeSet<String>) {
    compat_summary_with_attempts(
        mode,
        prefix,
        outcomes,
        attempts,
        &validate_corpus::known_failclosed(),
        &validate_corpus::portable_diagnostic(),
    )
}

/// The real summary body, with its two policy tables passed in.
///
/// Production calls it through [`print_compat_summary`] with the REAL tables, so nothing is
/// weakened; the tables are parameters purely so a bracket can exercise this exact code against
/// a planted table. That matters here because the shipped `known_failclosed()` currently holds a
/// single row, which is not enough to distinguish "listed and blocking" from "listed and
/// exempt" in one run -- and the fix for that must not be to add a fake row to production.
fn compat_summary_with_tables(
    mode: CompatMode,
    prefix: &str,
    outcomes: &[StepOutcome],
    known: &BTreeMap<&'static str, &'static str>,
    diag: &BTreeMap<&'static str, &'static str>,
) -> (usize, usize, Vec<String>, BTreeSet<String>) {
    compat_summary_with_attempts(mode, prefix, outcomes, &[], known, diag)
}

fn compat_summary_with_attempts(
    mode: CompatMode,
    prefix: &str,
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
    known: &BTreeMap<&'static str, &'static str>,
    diag: &BTreeMap<&'static str, &'static str>,
) -> (usize, usize, Vec<String>, BTreeSet<String>) {
    let mut per_cat: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut passed = 0usize;
    let mut measured = 0usize;
    let mut blocking_failures: Vec<String> = Vec::new();
    let mut nonblocking_failure_tags: BTreeSet<String> = BTreeSet::new();
    let mut measured_labels: BTreeSet<String> = BTreeSet::new();
    for o in outcomes {
        let Some(label) = o.tag.strip_prefix(prefix) else { continue };
        let classification = node_classification(o, attempts);
        if classification == NodeClassification::NoResult {
            println!(
                "  NO_RESULT {label} produced no product result; excluded from the measured denominator"
            );
            continue;
        }
        if matches!(classification, NodeClassification::UnderstoodInfrastructureFailure | NodeClassification::UnderstoodPrerequisiteFailure) {
            println!(
                "  NO_RESULT {label} could not determine its condition; excluded from the measured denominator"
            );
            continue;
        }
        let cat = validate_corpus::category_of(label);
        let e = per_cat.entry(cat).or_insert((0, 0));
        e.1 += 1;
        measured += 1;
        measured_labels.insert(label.to_string());
        if classification == NodeClassification::Pass {
            e.0 += 1;
            passed += 1;
        }
        // ONE decision, read twice: once for what to print and once for whether the row
        // blocks. Before this the two were separate arms of the same `if`, which is how a
        // reporting change can silently become an exemption.
        // `display_name()` deliberately renders Strict and PortableStrict identically, but these
        // two modes treat a listed row in OPPOSITE ways, so the message must distinguish them or
        // the reader cannot tell an exemption from a blocking report.
        let mode_label = match mode {
            CompatMode::Strict => "--strict",
            CompatMode::PortableStrict => "--portable-strict",
            other => other.display_name(),
        };
        let disposition = validate_plan::classify_compat_outcome(
            mode,
            classification == NodeClassification::Pass,
            known.contains_key(label),
            diag.contains_key(label),
        );
        match disposition {
            CompatDisposition::Passed => {}
            CompatDisposition::PassedButListedFailClosed => {
                println!(
                    "  WARN {label} passed but is listed as known fail-closed under {} \
                     ({}); the EXPECTATION is STALE -- drop it from the known-failure table",
                    mode_label,
                    known[label]
                );
            }
            CompatDisposition::KnownFailClosedExempt => {
                println!(
                    "  WARN {label} known fail-closed under --strict ({}; nonblocking)",
                    known[label]
                );
            }
            CompatDisposition::KnownFailClosedBlocking => {
                // Reported AND still blocking. Naming the reason is not excusing the failure:
                // the row is pushed onto `blocking_failures` below exactly as an unlisted
                // failure would be.
                println!(
                    "  FAIL {label} known fail-closed under {} ({}); STILL BLOCKING -- \
                     this mode does not exempt listed rows",
                    mode_label,
                    known[label]
                );
            }
            CompatDisposition::PortableDiagnostic => {
                println!("  WARN {label} is a bounded portable diagnostic: {}", diag[label]);
            }
            CompatDisposition::Blocking => {}
        }
        if disposition.is_blocking() {
            blocking_failures.push(label.to_string());
        } else if classification == NodeClassification::ProductFailure {
            nonblocking_failure_tags.insert(o.tag.clone());
        }
    }
    // AUDIT: a listed row the selected corpus never measured. Such a row is silently carried
    // forever -- it can neither fail (nothing ran it) nor be reported stale (it never passed),
    // so the table grows entries no run can retire. Naming them is reporting only; it changes
    // no verdict.
    if matches!(mode, CompatMode::Strict | CompatMode::PortableStrict) {
        let unmeasured: Vec<&str> = known
            .keys()
            .copied()
            .filter(|label| !measured_labels.contains(*label))
            .collect();
        if !unmeasured.is_empty() {
            println!(
                "  WARN {} known fail-closed row(s) not measured by this corpus, so no run can \
                 confirm or retire them: {}",
                unmeasured.len(),
                unmeasured.join(", ")
            );
        }
    }
    println!("\nCOMPATIBILITY SUMMARY ({measured} measured programs, mode {})", mode.display_name());
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
    (passed, measured, blocking_failures, nonblocking_failure_tags)
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

/// Execution completeness applies after profile-specific failure policy. A
/// profile may allow a fully measured failing row, but no profile may turn a
/// partial run into exit zero.
fn exit_code_with_execution_completeness(exit_code: u8, execution_complete: bool) -> u8 {
    if execution_complete || exit_code != 0 { exit_code } else { NO_RESULT_EXIT_CODE as u8 }
}

/// A missing pin gate invalidates a passing receipt, not an explicitly
/// off-the-record selected run. Selected hosted jobs inherit the exact commit
/// and a successful preflight through the external workflow dependency, and
/// they are already forbidden from writing a ledger row or publishing a
/// receipt. Turning a completed selected step into failure here would discard
/// its result without strengthening any evidence claim.
fn pin_gate_blocks_pass(exit_code: u8, pin_gate_passed: bool, off_the_record: bool) -> bool {
    exit_code == 0 && !pin_gate_passed && !off_the_record
}

/// Fast exit 127 is useful missing-artifact guidance only in `--only`, whose
/// documented contract deliberately drops build dependencies. It is never a
/// verdict override and is not inferred for full or other focused profiles.
fn possible_missing_artifact_nodes<'a>(
    selection_mode: &str,
    outcomes: &'a [StepOutcome],
) -> Vec<&'a str> {
    if selection_mode != "only" {
        return Vec::new();
    }
    outcomes
        .iter()
        .filter(|outcome| {
            !outcome.ok && outcome.returncode == Some(127) && outcome.duration_s < 5.0
        })
        .map(|outcome| outcome.tag.as_str())
        .collect()
}

/// Two-sided bracket for withholding a manifest bucket NODE whose entire cell
/// population is withheld.
///
/// Local and side-effect-free: it combines planted capability verdicts with the
/// checked-in manifests and production plan construction, but runs no probe or
/// scheduler and does not depend on which machine runs it. The load-bearing case
/// is UN-WITHHOLDING: give the same bucket one runnable cell back and the node
/// must run again, with no code change anywhere. That is the
/// difference between a computed decision and a hard-coded list of nodes that
/// would silently swallow a cell added later.
fn manifest_node_vacuity_profile_bracket(
    root: &Path,
    absent: &BTreeMap<validate_plan::HostCapability, String>,
    label: &str,
    withheld_tag: &str,
    committed_scorecard_tag: &str,
) -> Result<(), String> {
    let steps = validate_plan::lane_config(root, label)?.steps;
    let shipped = steps
        .iter()
        .find(|step| step.tag() == withheld_tag)
        .ok_or_else(|| format!("node vacuity: {label} selection lost bucket {withheld_tag}"))?;
    if manifest_bucket_of(shipped)
        != Some(("privileged".to_string(), "backend-parity-c".to_string()))
    {
        return Err(format!(
            "node vacuity: {withheld_tag} must bind to privileged/backend-parity-c; got {:?}",
            manifest_bucket_of(shipped)
        ));
    }

    let before = steps
        .iter()
        .map(|step| (step.tag(), step.deps.clone()))
        .collect::<BTreeMap<_, _>>();
    {
        let consumer = committed_scorecard_tag;
        if !before
            .get(consumer)
            .is_some_and(|deps| deps.iter().any(|dep| dep == withheld_tag))
        {
            return Err(format!(
                "node vacuity: {label} result consumer {consumer} does not depend on {withheld_tag}"
            ));
        }
    }
    let mut expected = before.clone();
    expected.remove(withheld_tag);
    {
        let consumer = committed_scorecard_tag;
        let deps = expected
            .get_mut(consumer)
            .ok_or_else(|| format!("node vacuity: {label} lost result consumer {consumer}"))?;
        deps.retain(|dep| dep != withheld_tag);
    }
    let mut actual = Plan {
        cfg: DagConfig {
            steps,
            ..Default::default()
        },
        ..Default::default()
    };
    withhold_vacuous_manifest_nodes(root, &mut actual, absent)?;
    let after = actual
        .cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step.deps.clone()))
        .collect::<BTreeMap<_, _>>();
    if after != expected
        || actual.host_inapplicable.len() != 1
        || actual.host_inapplicable[0].tag != withheld_tag
    {
        return Err(format!(
            "node vacuity: {label} withholding changed the wrong graph state: {after:?}"
        ));
    }
    Ok(())
}

fn node_vacuity_bracket(root: &Path) -> Result<(), String> {
    let bucket = |selected: usize, withheld: usize| BucketCells {
        lane: "privileged".into(),
        category: "backend-parity-c".into(),
        selected,
        withheld,
        capabilities: if withheld > 0 {
            vec!["cpuid-faulting".into()]
        } else {
            Vec::new()
        },
    };

    // POSITIVE — the bucket's only cell is withheld, so its node has nothing at
    // all left to run.
    if !bucket_runs_nothing(&bucket(1, 1)) {
        return Err("node vacuity: a bucket whose every selected cell is withheld must \
                    withhold its node"
            .into());
    }
    // ...and the same at a larger size, so the rule is not "exactly one cell".
    if !bucket_runs_nothing(&bucket(78, 78)) {
        return Err("node vacuity: an all-withheld bucket of 78 cells must withhold its node".into());
    }

    // THE UN-WITHHOLDING PROOF. One runnable cell in the bucket and the node
    // runs, however many of its siblings are withheld. Nothing is edited to make
    // this happen: the same predicate over a changed cell population produces
    // the opposite answer.
    for (selected, withheld) in [(2usize, 1usize), (79, 78), (100, 99)] {
        if bucket_runs_nothing(&bucket(selected, withheld)) {
            return Err(format!(
                "node vacuity: a bucket with {} runnable cell(s) left ({selected} selected, \
                 {withheld} withheld) must still RUN its node; withholding it would silently \
                 swallow the runnable cells",
                selected - withheld
            ));
        }
    }

    // NEGATIVE — nothing withheld at all.
    for selected in [1usize, 5, 78] {
        if bucket_runs_nothing(&bucket(selected, 0)) {
            return Err("node vacuity: a bucket with nothing withheld must run its node".into());
        }
    }

    // NEGATIVE, AND SEPARATELY LOAD-BEARING — an EMPTY bucket is the
    // pre-existing `empty-manifest-bucket` condition, not this one. Without the
    // `selected > 0` guard, `0 == 0` would make every empty bucket read as
    // host-inapplicable and quietly inflate the omission count.
    if bucket_runs_nothing(&bucket(0, 0)) {
        return Err("node vacuity: a bucket that selected NO cells is empty-manifest-bucket, \
                    never host-inapplicable"
            .into());
    }

    // THE NODE-TO-BUCKET BINDING. The typed manifest value is authoritative;
    // the command is checked only to prove that execution selects the same
    // population. A command with any selection token this function does not
    // model must NOT be matched against the bucket accounting.
    let mut shipped_buckets = Vec::new();
    for (label, tag) in [
        ("full", "privileged-e2e.manifest_backend_parity_c"),
        (
            "privileged",
            "privileged-only-e2e.manifest_backend_parity_c",
        ),
    ] {
        let steps = validate_plan::lane_config(root, label)?.steps;
        let shipped = steps
            .iter()
            .find(|step| step.tag() == tag)
            .ok_or_else(|| {
                format!("node vacuity: {label} selection lost privileged bucket {tag}")
            })?
            .clone();
        if manifest_bucket_of(&shipped)
            != Some(("privileged".to_string(), "backend-parity-c".to_string()))
        {
            return Err(format!(
                "node vacuity: shipped bucket {tag} must bind to privileged/backend-parity-c; got {:?}",
                manifest_bucket_of(&shipped)
            ));
        }
        shipped_buckets.push(shipped);
    }
    let unmodelled = [
        // A narrower selection than the accounting was taken with.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --mode verify --results r --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --backend ptrace --results r --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --test backend-parity-c/cpuid-probe --results r --junit j",
        // A WIDER selection than the accounting was taken with.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --include-occasional --results r --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --results r --junit j",
        // Output paths are required exactly once, and each must have a value.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r1 --results r2 --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r --junit j1 --junit j2",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r --junit",
        // Unknown tokens remain fail-closed even with the required output pair.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --future-selector value --results r --junit j",
        // Not a bucket run at all.
        "target/debug/test-harness validate",
        "target/debug/test-harness build --lane privileged --ci-only --allow-empty",
        "cargo test -p hermit-detcore",
    ];
    for shipped in &shipped_buckets {
        for cmd in unmodelled {
            let mut step = shipped.clone();
            step.cmd = cmd.to_string();
            if manifest_bucket_of(&step).is_some() {
                return Err(format!(
                    "node vacuity: {cmd:?} selects a cell population this function cannot prove \
                     equal to the bucket accounting and must NOT be a withholding candidate"
                ));
            }
        }
        let mut mismatched = shipped.clone();
        mismatched.manifest = Some(DagManifest {
            lane: "portable".into(),
            category: "backend-parity-c".into(),
            test: None,
            mode: None,
            backend: None,
        });
        if manifest_bucket_of(&mismatched).is_some() {
            return Err(
                "node vacuity: a command and typed manifest that name different lanes must refuse"
                    .into(),
            );
        }
        let mut untyped = shipped.clone();
        untyped.manifest = None;
        if manifest_bucket_of(&untyped).is_some() {
            return Err(
                "node vacuity: command text alone must not supply manifest lane/category".into(),
            );
        }
    }

    // THE CHECKED-IN ACCOUNTING — the required plan itself carries the host
    // capability metadata generated from the live YAML manifests. This must be
    // enough to withhold the current privileged bucket without compiling or
    // invoking another validation driver before dagrun starts.
    let absent = BTreeMap::from([(
        validate_plan::HostCapability::CpuidFaulting,
        "planted absence".to_string(),
    )]);
    let parsed = read_bucket_cells(root, &absent)?;
    let privileged = parsed
        .iter()
        .find(|bucket| bucket.lane == "privileged" && bucket.category == "backend-parity-c")
        .ok_or("node vacuity: required plan lost the privileged backend-parity-c bucket")?;
    if privileged.selected != 2
        || !bucket_runs_nothing(privileged)
        || privileged.capabilities != vec!["cpuid-faulting".to_string()]
    {
        return Err(format!(
            "node vacuity: checked-in host-capability accounting is wrong: {privileged:?}"
        ));
    }
    let present = read_bucket_cells(root, &BTreeMap::new())?;
    if present.iter().any(|bucket| bucket.withheld != 0) {
        return Err(
            "node vacuity: no bucket may be withheld when no capability is absent".into(),
        );
    }

    // INTEGRATION: exercise both committed tag families through production
    // accounting, withholding, and scorecard-edge handling.
    for (label, withheld_tag, scorecard_tag) in [
        (
            "full",
            "privileged-e2e.manifest_backend_parity_c",
            "full-scorecard.compatibility",
        ),
        (
            "privileged",
            "privileged-only-e2e.manifest_backend_parity_c",
            "privileged-scorecard.compatibility",
        ),
    ] {
        manifest_node_vacuity_profile_bracket(
            root,
            &absent,
            label,
            withheld_tag,
            scorecard_tag,
        )?;
    }

    // THE RETAINED DEPENDENT. A result consumer keeps running with the edge
    // dropped; ANY other dependent refuses the whole run rather than having a
    // prerequisite quietly removed from under it.
    let gone: BTreeSet<String> = ["privileged-e2e.manifest_backend_parity_c".to_string()]
        .into_iter()
        .collect();
    let consumer = (
        "scorecard.compatibility".to_string(),
        "./ci/compat-envelope/scorecard.rs verify-results --results \"$E2E_RESULT_ROOT\" \
         --lanes portable,privileged"
            .to_string(),
        vec![
            "privileged-e2e.manifest_backend_parity_c".to_string(),
            "e2e.manifest_util_c".to_string(),
        ],
    );
    let prerequisite = (
        "test.something".to_string(),
        "cargo nextest run -p hermit-detcore".to_string(),
        vec!["privileged-e2e.manifest_backend_parity_c".to_string()],
    );
    let unrelated = (
        "lint.rustfmt".to_string(),
        "cargo fmt --all -- --check".to_string(),
        vec!["quick.build".to_string()],
    );
    let (droppable, refusals) =
        classify_withheld_dependents(&[consumer.clone(), unrelated.clone()], &gone);
    if droppable
        != vec![(
            "scorecard.compatibility".to_string(),
            "privileged-e2e.manifest_backend_parity_c".to_string(),
        )]
        || !refusals.is_empty()
    {
        return Err(format!(
            "node vacuity: exactly the result consumer's edge to the withheld node may be \
             dropped; got droppable={droppable:?} refusals={refusals:?}"
        ));
    }
    let (droppable, refusals) =
        classify_withheld_dependents(&[prerequisite.clone(), unrelated.clone()], &gone);
    if !droppable.is_empty() || refusals.len() != 1 {
        return Err(format!(
            "node vacuity: a NON-result-consuming dependent must REFUSE the run, never have its \
             prerequisite silently removed; got droppable={droppable:?} refusals={refusals:?}"
        ));
    }
    // Nothing withheld: no edge is touched and nothing refuses.
    let (droppable, refusals) =
        classify_withheld_dependents(&[consumer, prerequisite, unrelated], &BTreeSet::new());
    if !droppable.is_empty() || !refusals.is_empty() {
        return Err("node vacuity: with nothing withheld, no dependency edge may change".into());
    }

    println!(
        "  node vacuity: 2 withheld / 7 not-withheld (3 un-withholding, 3 nothing-withheld, \
         1 empty-bucket), 1 transformed command bound / 15 refused, accounting parser 1 good / \
         3 malformed, dependents 1 edge-dropped / 1 refusal / 1 inert, actual plan 1 bucket \
         withheld / 1 scorecard edge dropped / 0 other changes"
    );
    Ok(())
}

/// Two-sided bracket for the host-capability withholding decision.
///
/// Inert: it plants capability verdicts instead of probing, so it exercises the
/// decision on BOTH the "machine cannot run it" and the "machine can run it"
/// side without depending on which machine is running the bracket. The
/// load-bearing case is NEGATIVE 2: a node that is merely BROKEN must never be
/// withheld, whatever is absent.
fn host_capability_bracket(root: &Path) -> Result<(), String> {
    use validate_plan::HostCapability;
    let step = |group: &str, job: &str, deps: Vec<String>| dagrun::model::Step {
        group: group.into(),
        job: job.into(),
        desc: String::new(),
        description: String::new(),
        cmd: "true".into(),
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        labels: Vec::new(),
        deps,
        env: BTreeMap::new(),
        hint: dagrun::model::ResourceHint::default(),
        networkonly: false,
        engine_only: false,
        timeout: 10,
        cpu_timeout: 10,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    };
    let requirements: BTreeMap<String, HostCapability> =
        [("cpuid.faulting".to_string(), HostCapability::CpuidFaulting)].into_iter().collect();
    let absent: BTreeMap<HostCapability, String> =
        [(HostCapability::CpuidFaulting, "planted".to_string())].into_iter().collect();
    let present: BTreeMap<HostCapability, String> = BTreeMap::new();
    let plan = || {
        vec![
            step("cpuid", "faulting", vec!["build.privileged_tests".into()]),
            step("test", "detcore_unit", vec!["build.privileged_tests".into()]),
            step("build", "privileged_tests", vec![]),
        ]
    };

    // POSITIVE — the declaring node, and only it, is withheld when the machine
    // provably lacks the capability.
    let (keep, gone) = validate_plan::partition_host_inapplicable(plan(), &requirements, &absent)?;
    if gone.len() != 1 || gone[0].tag != "cpuid.faulting" {
        return Err(format!(
            "host capability: exactly cpuid.faulting must be withheld, got {:?}",
            gone.iter().map(|n| n.tag.clone()).collect::<Vec<_>>()
        ));
    }
    if keep.len() != 2 {
        return Err("host capability: withholding one node must not remove any other".into());
    }

    // NEGATIVE 1 — with the capability present, nothing is withheld. Without
    // this the mechanism could be a blanket omission rather than a predicate.
    let (keep, gone) = validate_plan::partition_host_inapplicable(plan(), &requirements, &present)?;
    if !gone.is_empty() || keep.len() != 3 {
        return Err("host capability: a capable machine must run every planned node".into());
    }

    // NEGATIVE 2 — THE ONE THAT MATTERS. A node that declares NO capability is
    // never withheld, whatever is absent. This is what stops the mechanism from
    // being usable to excuse a node that is merely broken: a broken node has no
    // declaration, so it still runs, still fails, and is still refused.
    let undeclared: BTreeMap<String, HostCapability> = BTreeMap::new();
    let (keep, gone) = validate_plan::partition_host_inapplicable(plan(), &undeclared, &absent)?;
    if !gone.is_empty() || keep.len() != 3 {
        return Err(
            "host capability: an undeclared node was withheld; an absent capability must not \
             excuse a node that never claimed to need it"
                .into(),
        );
    }

    // NEGATIVE 3 — withholding a node that a RETAINED node depends on is a
    // refusal, not a silent cascade of unrun work.
    let mut dependent = plan();
    dependent.push(step("e2e", "needs_cpuid", vec!["cpuid.faulting".into()]));
    if validate_plan::partition_host_inapplicable(dependent, &requirements, &absent).is_ok() {
        return Err(
            "host capability: withholding a node with a retained dependent must REFUSE".into()
        );
    }

    // VOCABULARY — closed on both sides of the parse.
    if HostCapability::from_value("cpuid-faulting") != Some(HostCapability::CpuidFaulting) {
        return Err("host capability: the shipped capability name must parse".into());
    }
    if HostCapability::from_value("cpuid_faulting").is_some()
        || HostCapability::from_value("anything-at-all").is_some()
    {
        return Err("host capability: an unrecognized capability name must NOT parse".into());
    }

    // NON-VACUITY — the shipped DAG really does declare the requirement this
    // bracket is about, and every declaration in every lane parses. A bracket
    // that passed against an empty declaration set would prove nothing.
    let shipped = validate_plan::host_capability_requirements(root)?;
    if shipped.get("privileged-cpuid.faulting") != Some(&HostCapability::CpuidFaulting)
        || shipped.get("privileged-only-cpuid.faulting") != Some(&HostCapability::CpuidFaulting)
    {
        return Err(format!(
            "host capability: ci/dag/validate.json must label both full and privileged \
             CPUID nodes as requiring cpuid-faulting; got {shipped:?}"
        ));
    }

    // THE PROBE'S OWN CONJUNCTION, bracketed with planted observations so it is
    // checked on a machine of either kind. Exactly ONE combination may read as
    // absent; every form of doubt must run the node.
    let absent_cases = [
        // (syscall, /proc/cpuinfo advertises cpuid_fault, must read absent)
        (Err(libc::ENODEV), Some(false), true),
        // The kernel accepted it: present however cpuinfo reads.
        (Ok(()), Some(false), false),
        (Ok(()), Some(true), false),
        // The two sources DISAGREE — doubt, so the node runs.
        (Err(libc::ENODEV), Some(true), false),
        // /proc/cpuinfo unreadable — doubt, so the node runs.
        (Err(libc::ENODEV), None, false),
        // A different errno is doubt about the PROBE, not proof about the
        // machine. EPERM is what a restricted sandbox returns.
        (Err(libc::EPERM), Some(false), false),
        (Err(libc::EINVAL), Some(false), false),
        // The fork/waitpid probe could not be completed at all.
        (Err(0), Some(false), false),
    ];
    for (syscall, advertised, want_absent) in absent_cases {
        if validate_plan::cpuid_faulting_absent(syscall, advertised) != want_absent {
            return Err(format!(
                "host capability: cpuid-faulting absence for (syscall={syscall:?}, \
                 cpuinfo={advertised:?}) must be {want_absent}; only a corroborated ENODEV may \
                 read as absent and every other shape must run the node"
            ));
        }
    }

    // The same conjunction for KVM, and the same rule: only a corroborated
    // ENOENT may read as absent. ⚠️ For this capability "doubt runs the node" is
    // only safe because the node also asserts its executed-test COUNT -- every
    // `run_kvm_` test self-guards on /dev/kvm and returns early, so a wrongly-run
    // node would report silent passes rather than a loud failure.
    let kvm_absent_cases: &[(Result<(), i32>, Option<bool>, bool)] = &[
        // The only shape that may read as absent: no device AND no vmx/svm.
        (Err(libc::ENOENT), Some(false), true),
        // The device opened: present, whatever /proc/cpuinfo says.
        (Ok(()), Some(false), false),
        (Ok(()), Some(true), false),
        // The two sources DISAGREE -- doubt, so the node runs.
        (Err(libc::ENOENT), Some(true), false),
        // /proc/cpuinfo unreadable -- doubt, so the node runs.
        (Err(libc::ENOENT), None, false),
        // A restricted sandbox or a permissions problem is doubt about the
        // PROBE, not proof the machine lacks KVM.
        (Err(libc::EACCES), Some(false), false),
        (Err(libc::EPERM), Some(false), false),
        (Err(libc::EBUSY), Some(false), false),
    ];
    for (open, advertised, want_absent) in kvm_absent_cases {
        if validate_plan::kvm_absent(*open, *advertised) != *want_absent {
            return Err(format!(
                "host capability: kvm absence for (open={open:?}, cpuinfo={advertised:?}) must be \
                 {want_absent}; only a corroborated ENOENT may read as absent and every other \
                 shape must run the node"
            ));
        }
    }

    node_vacuity_bracket(root)?;

    // The one override can only force PRESENT; nothing forces ABSENT.
    let verdict = validate_plan::probe_host_capability(HostCapability::CpuidFaulting);
    println!(
        "  host capability: 1 withheld / 3 not-withheld (capable, undeclared, dependent-refusal), \
         probe conjunction 1 absent / 7 present-on-doubt, vocabulary closed, shipped DAG declares \
         it; this machine's cpuid-faulting probe says {} ({})",
        if verdict.present { "PRESENT" } else { "ABSENT" },
        verdict.evidence
    );
    Ok(())
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

/// Both sides of [`pin_gate_blocks_pass`], using only planted booleans.
fn pin_gate_receipt_bracket() -> Result<(), String> {
    if !pin_gate_blocks_pass(0, false, false) {
        return Err("pin gate: a receipt-producing pass without the gate was accepted".into());
    }
    for (exit_code, pin_gate_passed, off_the_record, label) in [
        (0, true, false, "receipt-producing pass with gate"),
        (1, false, false, "existing failure without gate"),
        (0, false, true, "off-the-record selected pass without gate"),
    ] {
        if pin_gate_blocks_pass(exit_code, pin_gate_passed, off_the_record) {
            return Err(format!("pin gate: {label} was incorrectly refused"));
        }
    }
    println!(
        "  pin gate: receipt-producing pass requires the observed gate; off-the-record selected pass does not claim a receipt"
    );
    Ok(())
}

fn scorecard_writeback_scope_bracket() -> Result<(), String> {
    if !should_write_scorecard(false, false)
        || should_write_scorecard(true, false)
        || should_write_scorecard(false, true)
        || should_write_scorecard(true, true)
    {
        return Err(
            "scorecard write-back: only a receipt-producing top-level run may update the tracked projection"
                .into(),
        );
    }
    println!(
        "  scorecard write-back: receipt-producing top-level run only; nested and off-the-record runs inert"
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

/// Give every validation node an explicit fail-fast family.
///
/// A node that already names a shared family keeps it. Every other node gets its own tag, so its
/// failure still blocks true dependents through the DAG edges without cancelling unrelated work.
/// The runner's global eager-exit default remains available to every graph that does not opt in.
fn assign_fail_fast_families(plan: &mut Plan) {
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        for step in &mut cfg.steps {
            if step.fail_fast_family.is_none() {
                step.fail_fast_family = Some(step.tag());
            }
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

/// One outer scheduler attempt for one DAG node.
///
/// Validate launches each outer node once. Framework-owned retries remain in
/// the node's typed `test_results`; retaining the attempt row keeps unknown,
/// aborted, environmental, and producer-owned failure evidence explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptExecution {
    /// The child spawned and wait produced its exit status.
    Completed,
    /// No child result exists: unreported, aborted, spawn failure, or supervisor failure.
    Unknown,
}

impl AttemptExecution {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Unknown => "unknown",
        }
    }
}

/// The closed reason associated with a retained retry observation.
///
/// Human detail is stored separately on the same attempt. Keeping the class as
/// an enum prevents a changing timeout, exit status, or registry sample from
/// turning one retry category into many unrelated strings in the ledger.
///
/// This is `gates[].attempts[].retry_class`, not the parent
/// `ci-hub/validate/retry_class.py` run-level value (`permanent`, `transient`,
/// or `no-result`). They answer different questions and share only the field
/// name.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RetryClass {
    AlwaysEligible,
    BpfjailerBanner,
}

impl RetryClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::AlwaysEligible => "always-eligible",
            Self::BpfjailerBanner => "bpfjailer-banner",
        }
    }
}

/// Typed execution state for a scheduler outcome.
///
/// `reported && !aborted` is insufficient: spawn failures and supervisor
/// crashes deliberately publish a non-aborted failure row, but both say the
/// step's own result is UNKNOWN and carry no child exit status.
fn outcome_execution(outcome: &StepOutcome) -> AttemptExecution {
    if !outcome.aborted && outcome.returncode.is_some() {
        AttemptExecution::Completed
    } else {
        AttemptExecution::Unknown
    }
}

#[derive(Clone)]
struct NodeAttempt {
    /// The node's `group.job` tag.
    tag: String,
    /// 1-based ordinal. Attempt 1 is the first scheduler pass.
    attempt: usize,
    /// The scheduler's safety verdict when it reported this attempt. Pair this
    /// with `execution`: spawn/supervisor failures report `Some(false)` so the
    /// run fails closed, while the step's own result remains UNKNOWN.
    ok: Option<bool>,
    /// Whether a completion payload arrived at all. False is the
    /// verdict-not-recorded case that a re-run cannot distinguish after the
    /// fact unless it is written down here.
    reported: bool,
    returncode: Option<i64>,
    /// Typed scheduler termination facts. `None` means no completion payload
    /// arrived; it must not be flattened into false or zero.
    oomed: Option<bool>,
    oom_kills: Option<i64>,
    timed_out: Option<bool>,
    cpu_timed_out: Option<bool>,
    /// The runner's typed failure reason for THIS attempt; `""` when it passed
    /// or was never reported. A later attempt never overwrites it.
    reason: String,
    duration_s: f64,
    aborted: bool,
    /// Whether a child actually executed through a collected exit status.
    execution: AttemptExecution,
    /// Historical outer retry class, or the class attached to a synthesized
    /// compatibility observation. New outer scheduler attempts leave this null;
    /// framework-owned retries are represented in `test_results`.
    retry_class: Option<RetryClass>,
    /// Historical evidence behind the class when it is not already the
    /// attempt's `reason`; it never changes the grouping key.
    retry_detail: Option<String>,
    /// The environmental signature found in this failed attempt's own detail
    /// region. This is kept separately from `retry_class`: a classified attempt
    /// may never execute again, and that distinction is the UNCONFIRMED verdict.
    environmental_class: Option<String>,
    /// A strict terminal infrastructure signature from this node's own detail.
    /// Broader environmental hypotheses and raw failure attribution remain
    /// separate; a measured product failure always takes precedence.
    understood_infrastructure_class: Option<String>,
    /// Whether this attempt's own round emitted a detail region, even if that
    /// region carried no environmental signature. Without this bit, "banner
    /// gone" is indistinguishable from "no new evidence was captured".
    detail_observed: bool,
    /// The producer-owned attribution of this terminal attempt. This is a
    /// closed value; `reason` and `failure_detail` remain explanatory text and
    /// never decide the class in an outer ledger reader.
    failure_class: Option<FailureClass>,
    /// The exact recognized evidence value for a non-product class. Product
    /// failures keep their existing human `reason` without manufacturing a
    /// second description.
    failure_detail: Option<String>,
    /// Terminal per-test results written by a controlled runner for this exact attempt.
    /// `None` means no typed result file was published; an empty vector is measured zero.
    test_results: Option<Vec<dagrun::TestResult>>,
}

fn attempt_is_no_result(attempt: &NodeAttempt) -> bool {
    attempt.execution == AttemptExecution::Completed
        && !attempt.aborted
        && attempt.returncode == Some(NO_RESULT_EXIT_CODE)
}

fn attempt_result(attempt: &NodeAttempt) -> Option<&'static str> {
    if attempt.execution != AttemptExecution::Completed {
        None
    } else if attempt.ok == Some(true) {
        Some("pass")
    } else if attempt_is_no_result(attempt) {
        Some("no_result")
    } else if attempt.ok == Some(false) {
        Some("fail")
    } else {
        None
    }
}

fn attempt_is_failure(attempt: &NodeAttempt) -> bool {
    attempt_result(attempt) == Some("fail")
}

fn outcome_failure_class(outcome: &StepOutcome) -> Option<FailureClass> {
    if outcome.ok || outcome.aborted {
        None
    } else if outcome_is_no_result(outcome)
        || outcome.returncode.is_none()
        || outcome_hit_its_budget(outcome)
        || outcome.oomed
    {
        Some(FailureClass::NoResult)
    } else {
        Some(FailureClass::ProductFailure)
    }
}

fn terminal_attempt<'a>(outcome: &StepOutcome, attempts: &'a [NodeAttempt]) -> Option<&'a NodeAttempt> {
    attempts.iter().rev().find(|attempt| attempt.tag == outcome.tag)
}

/// Nodes with at least one attempt that produced a child exit status.
///
/// `outcomes.len()` is the number of scheduler records, not the number of
/// executions: spawn failures, supervisor failures, and aborted peers all
/// deliberately produce records with `execution = unknown` so they cannot
/// disappear. Keeping the populations separate prevents retained evidence from
/// turning an unknown execution into a measured node.
fn completed_node_count(outcomes: &[StepOutcome], attempts: &[NodeAttempt]) -> usize {
    outcomes
        .iter()
        .filter(|outcome| {
            let mut node_attempts = attempts.iter().filter(|attempt| attempt.tag == outcome.tag);
            let Some(first) = node_attempts.next() else {
                return outcome_execution(outcome) == AttemptExecution::Completed;
            };
            first.execution == AttemptExecution::Completed
                || node_attempts.any(|attempt| attempt.execution == AttemptExecution::Completed)
        })
        .count()
}

/// Record one attempt the scheduler REPORTED.
fn reported_attempt(outcome: &StepOutcome, attempt: usize) -> NodeAttempt {
    let failure_class = outcome_failure_class(outcome);
    NodeAttempt {
        tag: outcome.tag.clone(),
        attempt,
        ok: Some(outcome.ok),
        reported: true,
        returncode: outcome.returncode,
        oomed: Some(outcome.oomed),
        oom_kills: Some(outcome.oom_kills),
        timed_out: Some(outcome.timed_out),
        cpu_timed_out: Some(outcome.cpu_timed_out),
        reason: outcome.reason.clone(),
        duration_s: outcome.duration_s,
        aborted: outcome.aborted,
        execution: outcome_execution(outcome),
        retry_class: None,
        retry_detail: None,
        environmental_class: None,
        understood_infrastructure_class: None,
        detail_observed: false,
        failure_detail: (failure_class == Some(FailureClass::NoResult)
            && !outcome.reason.is_empty())
            .then(|| outcome.reason.clone()),
        failure_class,
        test_results: outcome.test_results.clone(),
    }
}

/// Record one attempt for which NO completion payload arrived. Every observation
/// field stays absent rather than defaulting to a zero, because a fabricated
/// `exit 0`/`0.0s` here would read exactly like a node that ran and passed.
fn unreported_attempt(tag: String, attempt: usize) -> NodeAttempt {
    NodeAttempt {
        tag,
        attempt,
        ok: None,
        reported: false,
        returncode: None,
        oomed: None,
        oom_kills: None,
        timed_out: None,
        cpu_timed_out: None,
        reason: "no completion payload was reported for this node".into(),
        duration_s: 0.0,
        aborted: false,
        execution: AttemptExecution::Unknown,
        retry_class: None,
        retry_detail: None,
        environmental_class: None,
        understood_infrastructure_class: None,
        detail_observed: false,
        failure_class: Some(FailureClass::NoResult),
        failure_detail: Some("no completion payload was reported for this node".into()),
        test_results: None,
    }
}

/// The exact attempt whose failed detail region is represented by `by_tag`.
///
/// A later scheduler round can add an unreported or aborted row without adding
/// a new detail region. Selecting merely "latest by tag" would then attach the
/// old reported failure's signature to an attempt that produced no evidence.
fn latest_reported_failure_mut<'a>(
    attempts: &'a mut [NodeAttempt],
    tag: &str,
) -> Option<&'a mut NodeAttempt> {
    attempts.iter_mut().rev().find(|attempt| {
        attempt.tag == tag && attempt.reported && attempt_is_failure(attempt)
    })
}

/// Attach one round's detail observation to the exact reported failure it came from.
fn stamp_attempt_detail(
    attempts: &mut [NodeAttempt],
    tag: &str,
    environmental_class: Option<&str>,
    understood_infrastructure_class: Option<&str>,
    failure: Option<(FailureClass, &str)>,
) {
    if let Some(attempt) = latest_reported_failure_mut(attempts, tag) {
        attempt.detail_observed = true;
        attempt.environmental_class = environmental_class.map(str::to_string);
        attempt.understood_infrastructure_class = understood_infrastructure_class.map(str::to_string);
        if attempt.failure_class == Some(FailureClass::ProductFailure) {
            if let Some((failure_class, failure_detail)) = failure {
                attempt.failure_class = Some(failure_class);
                attempt.failure_detail = Some(failure_detail.to_string());
            }
        }
    }
}

/// The first later attempt that demonstrably executed and completed.
///
/// A retry round, an unreported row, or an aborted scheduler outcome is not an
/// execution result and cannot confirm or refute anything.
fn actual_rerun_after<'a>(
    attempts: &'a [NodeAttempt],
    classified: &NodeAttempt,
) -> Option<&'a NodeAttempt> {
    attempts
        .iter()
        .filter(|attempt| {
            attempt.tag == classified.tag
                && attempt.attempt > classified.attempt
                && attempt.execution == AttemptExecution::Completed
                && attempt.ok.is_some()
                && !attempt_is_no_result(attempt)
        })
        .min_by_key(|attempt| attempt.attempt)
}

/// Settle one classified attempt from the attempt ledger itself.
fn environmental_assessment(
    attempts: &[NodeAttempt],
    classified: &NodeAttempt,
) -> Option<(validate_runtime::EnvBlockVerdict, Option<validate_runtime::RefutedShape>)> {
    // NO GOALPOST LOWERING: this derives evidence after execution. It does not
    // alter retry eligibility, the terminal StepOutcome, LaneResult::ok, or
    // failure counts. Unknown execution can only tighten completeness and refuse
    // a receipt that lacked evidence; a label can never turn a RED into a pass.
    let original = classified.environmental_class.as_deref()?;
    let rerun = actual_rerun_after(attempts, classified);
    let rerun_result = rerun.and_then(|attempt| attempt.ok);
    let verdict = validate_runtime::EnvBlockVerdict::settle(rerun_result);
    let shape = if verdict == validate_runtime::EnvBlockVerdict::Refuted
        && rerun.is_some_and(|attempt| attempt.detail_observed)
    {
        Some(validate_runtime::RefutedShape::of(
            original,
            rerun.and_then(|attempt| attempt.environmental_class.as_deref()),
        ))
    } else {
        None
    };
    Some((verdict, shape))
}

/// One lane's terminal state after its single outer scheduler execution.
struct LaneResult {
    outcomes: Vec<StepOutcome>,
    skipped: Vec<String>,
    /// Every attempt of every node, in the order they were reported. A node run
    /// once contributes exactly one row, so this is a superset of `outcomes`
    /// rather than a parallel structure that can disagree with it.
    attempts: Vec<NodeAttempt>,
    /// Every non-intentional planned node completed with a collected child exit
    /// status, and the whole-run clock did not cut the lane short. Dependency-
    /// skipped, aborted, unreported, spawn-failed, and supervisor-failed nodes
    /// are incomplete. This is deliberately separate from node success: compat,
    /// super, and envelope profiles may allow a fully measured failing row.
    complete: bool,
    /// Whether every reported node succeeded or was aborted after a peer failed.
    /// This does not answer whether the planned lane was completely reported.
    ok: bool,
    /// The whole-invocation deadline expired during this lane.
    run_timed_out: bool,
}

/// Return the durable log's byte length once it has stopped growing.
///
/// The driver tees its own stdout/stderr through a `tee` child, so a node's
/// `----- detail -----` region reaches the file slightly after the runner emits
/// it. Flushing and waiting for a stable size before taking a watermark or a
/// slice keeps adjacent scheduler invocations from borrowing each other's
/// output.
fn settled_log_len(path: &Path) -> u64 {
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
    last
}

/// Read only bytes emitted since this scheduler invocation's start watermark.
///
/// A whole-file `rfind` can reuse attempt 1's banner when attempt 2 emits no
/// detail at all. An empty slice is therefore evidence of NO NEW REGION, not a
/// reason to look backwards. Truncation or unreadability is likewise unknown.
fn read_log_since_settled(path: &Path, start: u64) -> Option<String> {
    let end = settled_log_len(path);
    if end < start {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let start = usize::try_from(start).ok()?;
    if start > bytes.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

/// Forward this scheduler invocation's rows to the directory uploaded by the
/// hosted shard.
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
        // Each caller passes the rows of exactly one `run_dag_boxed_deadline`
        // execution, so a freshly minted run_id (`None`) groups precisely that
        // execution. An environmental retry is a separate execution and gets its
        // own id, which is what keeps the retry's rows distinguishable from the
        // first attempt's instead of merging both into one apparent run.
        None,
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
/// A nested focused fixture must spend from the enclosing scheduler step's
/// clock. Starting a new `Instant` after re-exec and setup made an inner bound
/// numerically smaller but temporally false.
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

/// Planned runnable steps absent from both scheduler result collections.
/// Whether the scheduler refused the whole lane before starting any node.
///
/// Every pre-flight refusal path returns empty outcomes, empty dependency skips, an empty
/// `not_launched`, AND empty intentional skips -- deliberately, because nothing was left
/// unlaunched *by a failure*. A planned lane that produced none of the four therefore never
/// started, and its nodes are explained by the refusal, not unaccounted for.
fn scheduler_refused_before_launching(
    planned: usize,
    outcomes: usize,
    skipped: usize,
    not_launched: usize,
    intentional_skips: usize,
) -> bool {
    planned > 0
        && outcomes == 0
        && skipped == 0
        && not_launched == 0
        && intentional_skips == 0
}

/// Replace the recorded reason a planned node did not run with the latest scheduler attempt.
///
/// A retry is a new attempt: a node that was not launched on attempt one may be refused
/// before attempt two, or may run on attempt two. Keeping the old reason would describe history
/// rather than the terminal attempt that makes the lane incomplete.
fn update_not_run_explanations(
    planned: &[String],
    outcomes: usize,
    skipped: usize,
    not_launched: &[String],
    intentional_skips: usize,
    scheduler_not_launched: &mut BTreeSet<String>,
    refused: &mut BTreeSet<String>,
) {
    for tag in planned {
        scheduler_not_launched.remove(tag);
        refused.remove(tag);
    }
    if scheduler_refused_before_launching(
        planned.len(),
        outcomes,
        skipped,
        not_launched.len(),
        intentional_skips,
    ) {
        refused.extend(planned.iter().cloned());
    } else {
        scheduler_not_launched.extend(not_launched.iter().cloned());
    }
}

/// Split nodes that produced no outcome into the two states they actually occupy:
/// those the scheduler returned in `not_launched` after admission stopped
/// (accounted for, without claiming whether fail-fast or the outer budget stopped it),
/// and those nothing explains.
///
/// Both still block a green lane. The distinction is diagnostic, and it is the
/// whole point: a deliberate skip that reads identically to a vanished node makes
/// every deliberate skip look like a defect and hides the real ones among them.
fn partition_unreported(
    unreported: &[String],
    not_launched: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    unreported
        .iter()
        .cloned()
        .partition(|tag| not_launched.contains(tag))
}

fn scheduler_not_launched_message(tags: &[String]) -> String {
    format!(
        "validate: {} planned node(s) DID NOT RUN; the scheduler returned them in \
         not_launched after it stopped admitting work (fail-fast or outer run budget): {}. \
         They are accounted for, but the lane remains incomplete and cannot be green.",
        tags.len(),
        tags.join(", ")
    )
}

#[cfg(test)]
mod scheduler_explanation_tests {
    use super::*;

    #[test]
    fn latest_attempt_replaces_the_prior_nonlaunch_reason() {
        let tag = "e2e.manifest_applications".to_string();
        let planned = vec![tag.clone()];
        let mut not_launched = BTreeSet::new();
        let mut refused = BTreeSet::new();

        update_not_run_explanations(
            &planned, 0, 0, std::slice::from_ref(&tag), 0, &mut not_launched, &mut refused,
        );
        assert!(not_launched.contains(&tag));
        assert!(!refused.contains(&tag));

        update_not_run_explanations(
            &planned, 0, 0, &[], 0, &mut not_launched, &mut refused,
        );
        assert!(!not_launched.contains(&tag));
        assert!(refused.contains(&tag));

        update_not_run_explanations(
            &planned, 1, 0, &[], 0, &mut not_launched, &mut refused,
        );
        assert!(!not_launched.contains(&tag));
        assert!(!refused.contains(&tag));
    }

    #[test]
    fn intentional_skip_is_not_a_preflight_refusal() {
        assert!(!scheduler_refused_before_launching(1, 0, 0, 0, 1));
        assert!(scheduler_refused_before_launching(1, 0, 0, 0, 0));
    }

    #[test]
    fn not_launched_diagnostic_does_not_invent_a_specific_cause() {
        let message = scheduler_not_launched_message(&["test.detcore_misc".to_string()]);
        assert_eq!(
            message,
            "validate: 1 planned node(s) DID NOT RUN; the scheduler returned them in not_launched after it stopped admitting work (fail-fast or outer run budget): test.detcore_misc. They are accounted for, but the lane remains incomplete and cannot be green."
        );
    }

}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NextestTimeoutCap {
    period_seconds: u64,
    terminate_after: u64,
    grace_seconds: u64,
}

fn nextest_duration_seconds(value: &toml::Value, field: &str) -> Result<u64, String> {
    let duration = value
        .as_str()
        .ok_or_else(|| format!("nextest {field} must be a duration string"))?;
    let digits = duration.strip_suffix('s').ok_or_else(|| {
        format!("nextest {field} must be a positive whole-second duration, got {duration:?}")
    })?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "nextest {field} must be a positive whole-second duration, got {duration:?}"
        ));
    }
    let seconds = digits
        .parse::<u64>()
        .map_err(|error| format!("invalid nextest {field} {duration:?}: {error}"))?;
    if seconds == 0 {
        return Err(format!("nextest {field} must be greater than zero"));
    }
    Ok(seconds)
}

fn parse_nextest_timeout_caps(source: &str) -> Result<Vec<NextestTimeoutCap>, String> {
    fn visit(value: &toml::Value, caps: &mut Vec<NextestTimeoutCap>) -> Result<(), String> {
        match value {
            toml::Value::Table(table) => {
                for (key, value) in table {
                    if key == "slow-timeout" {
                        let timeout = value
                            .as_table()
                            .ok_or("nextest slow-timeout must be a table")?;
                        let period_seconds = nextest_duration_seconds(
                            timeout
                                .get("period")
                                .ok_or("nextest slow-timeout is missing period")?,
                            "slow-timeout.period",
                        )?;
                        let terminate_after = timeout
                            .get("terminate-after")
                            .and_then(toml::Value::as_integer)
                            .and_then(|value| u64::try_from(value).ok())
                            .filter(|value| *value > 0)
                            .ok_or("nextest slow-timeout.terminate-after must be positive")?;
                        let grace_seconds = nextest_duration_seconds(
                            timeout
                                .get("grace-period")
                                .ok_or("nextest slow-timeout is missing grace-period")?,
                            "slow-timeout.grace-period",
                        )?;
                        caps.push(NextestTimeoutCap {
                            period_seconds,
                            terminate_after,
                            grace_seconds,
                        });
                    } else {
                        visit(value, caps)?;
                    }
                }
            }
            toml::Value::Array(values) => {
                for value in values {
                    visit(value, caps)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let document = source
        .parse::<toml::Value>()
        .map_err(|error| format!("cannot parse nextest config: {error}"))?;
    let mut caps = Vec::new();
    visit(&document, &mut caps)?;
    if caps.is_empty() {
        return Err("nextest config contains no slow-timeout".into());
    }
    Ok(caps)
}

fn render_scaled_nextest_config(root: &Path, multiplier: f64) -> Result<String, String> {
    let scratch = tempfile::tempdir()
        .map_err(|error| format!("cannot create nextest config scratch directory: {error}"))?;
    let output_path = scratch.path().join("nextest.toml");
    let output = Command::new(root.join("ci/nextest-timeout-config.rs"))
        .arg(root.join(".config/nextest.toml"))
        .arg(multiplier.to_string())
        .arg(&output_path)
        .output()
        .map_err(|error| format!("cannot run nextest timeout transformer: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "nextest timeout transformer exited {}: {}",
            output
                .status
                .code()
                .map_or_else(|| "by signal".into(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    std::fs::read_to_string(&output_path)
        .map_err(|error| format!("cannot read generated nextest config: {error}"))
}

fn require_matching_scaled_default(
    manifest_base_seconds: u64,
    multiplier: f64,
    nextest_caps: &[NextestTimeoutCap],
) -> Result<u64, String> {
    let expected = scale_timeout_seconds(manifest_base_seconds, multiplier, "wall multiplier")?;
    let actual = nextest_caps
        .first()
        .ok_or("generated nextest config contains no default timeout")?
        .period_seconds;
    if actual != expected {
        return Err(format!(
            "generated nextest default is {actual}s, but manifest {manifest_base_seconds}s scaled by {multiplier} with ceiling rounding is {expected}s"
        ));
    }
    Ok(expected)
}

fn require_outer_timeout_headroom(
    tag: &str,
    node_timeout_seconds: i64,
    base_inner_seconds: u64,
    termination_grace_seconds: u64,
    attempts: u64,
    wall_multiplier: f64,
) -> Result<i64, String> {
    let scaled_inner_seconds = scale_timeout_seconds(
        base_inner_seconds,
        wall_multiplier,
        &format!("{tag} wall multiplier"),
    )?;
    require_resolved_outer_timeout_headroom(
        tag,
        node_timeout_seconds,
        scaled_inner_seconds,
        1,
        termination_grace_seconds,
        attempts,
        &format!("at wall multiplier {wall_multiplier}"),
    )
}

fn require_resolved_outer_timeout_headroom(
    tag: &str,
    node_timeout_seconds: i64,
    resolved_inner_seconds: u64,
    wall_windows_per_attempt: u64,
    termination_grace_seconds: u64,
    attempts: u64,
    resolved_context: &str,
) -> Result<i64, String> {
    let node_timeout_seconds = u64::try_from(node_timeout_seconds)
        .map_err(|_| format!("retry bounds: {tag} has a nonpositive node timeout"))?;
    if attempts == 0 || resolved_inner_seconds == 0 {
        return Err(format!(
            "retry bounds: {tag} requires a positive attempt count and inner wall bound"
        ));
    }
    if wall_windows_per_attempt == 0 {
        return Err(format!(
            "retry bounds: {tag} has no inner wall-time window per attempt"
        ));
    }
    let bounded_wall_seconds = resolved_inner_seconds
        .checked_mul(wall_windows_per_attempt)
        .ok_or_else(|| format!("retry bounds: {tag} inner wall windows overflowed"))?;
    let one_attempt_seconds = bounded_wall_seconds
        .checked_add(termination_grace_seconds)
        .ok_or_else(|| format!("retry bounds: {tag} timeout plus grace overflowed"))?;
    let required_seconds = attempts
        .checked_mul(one_attempt_seconds)
        .ok_or_else(|| format!("retry bounds: {tag} attempt allowance overflowed"))?;
    if node_timeout_seconds <= required_seconds {
        return Err(format!(
            "retry bounds: {tag} has a {node_timeout_seconds}s node timeout but {attempts} inner attempt(s) {resolved_context} can consume {required_seconds}s ({wall_windows_per_attempt} x {resolved_inner_seconds}s wall windows plus {termination_grace_seconds}s termination grace each)"
        ));
    }
    i64::try_from(node_timeout_seconds - required_seconds)
        .map_err(|error| format!("retry bounds: {tag} headroom is too large: {error}"))
}

// Decode the known pinned-root shell transport before inspecting the actual
// harness argv. A filename or an outer-wrapper argument named --prebuilt must
// never remove the fixture-preparation window from timeout accounting.
fn manifest_command_source(tag: &str, command: &str) -> Result<String, String> {
    let mut source = command.to_owned();
    if command.starts_with("./ci/hermetic/run-in-pinned-root.sh ") {
        let argv = shell_words::split(command)
            .map_err(|error| format!("retry bounds: {tag} has invalid wrapper quoting: {error}"))?;
        let boundary = argv
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| format!("retry bounds: {tag} has no pinned-root command boundary"))?;
        let tail = &argv[boundary..];
        if tail.len() != 6 || tail[1] != "bash" || tail[2] != "-c" || tail[4] != "bash"
            || tail[3] != "/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c \"$1\""
        {
            return Err(format!("retry bounds: {tag} has an unrecognized pinned-root invocation"));
        }
        source = tail[5].clone();
    }
    let source = source
        .strip_prefix(RUST_SCRIPT_COMMAND_PREFIX)
        .unwrap_or(&source);
    let source = source
        .strip_prefix("./ci/run-with-hermit-e2e-artifact.sh ")
        .map(|inner| inner.strip_prefix("--require-install ").unwrap_or(inner))
        .unwrap_or(source);
    Ok(source.to_owned())
}

fn manifest_command_policy(tag: &str, command: &str) -> Result<(Selection, bool), String> {
    let source = manifest_command_source(tag, command)?;
    let argv = shell_words::split(&source)
        .map_err(|error| format!("retry bounds: {tag} has invalid harness quoting: {error}"))?;
    if argv.first().map(String::as_str) != Some("target/debug/test-harness")
        || argv.get(1).map(String::as_str) != Some("run")
    {
        return Err(format!(
            "retry bounds: manifest node {tag} does not invoke target/debug/test-harness run"
        ));
    }
    let mut selection = Selection::default();
    let mut prebuilt = false;
    let mut seen = BTreeSet::new();
    let mut index = 2;
    while let Some(option) = argv.get(index) {
        if !seen.insert(option.as_str()) {
            return Err(format!(
                "retry bounds: {tag} supplies {option} more than once"
            ));
        }
        match option.as_str() {
            "--ci-only" => selection.population = Some(Population::Required),
            "--prebuilt" => prebuilt = true,
            "--allow-empty" => {}
            "--lane" | "--category" | "--test" | "--mode" | "--backend" | "--results"
            | "--junit" | "--jobs" => {
                index += 1;
                let value = argv
                    .get(index)
                    .ok_or_else(|| format!("retry bounds: {tag} lacks a value for {option}"))?;
                match option.as_str() {
                    "--lane" => selection.lane = Some(value.clone()),
                    "--category" => selection.category = Some(value.clone()),
                    "--test" => selection.test = Some(value.clone()),
                    "--mode" => selection.mode = Some(value.clone()),
                    "--backend" => selection.backend = Some(value.clone()),
                    _ => {}
                }
            }
            _ => {
                return Err(format!(
                    "retry bounds: {tag} has an unmodeled harness argument {option:?}"
                ))
            }
        }
        index += 1;
    }
    if selection.population != Some(Population::Required) || selection.lane.is_none() {
        return Err(format!(
            "retry bounds: {tag} must select a named lane with --ci-only"
        ));
    }
    Ok((selection, prebuilt))
}

#[cfg(test)]
fn manifest_command_is_prebuilt(tag: &str, command: &str) -> Result<bool, String> {
    manifest_command_policy(tag, command).map(|(_, prebuilt)| prebuilt)
}

fn manifest_step_policy(step: &Step) -> Result<(Selection, bool), String> {
    let (selection, prebuilt) = manifest_command_policy(&step.tag(), &step.cmd)?;
    if let Some(manifest) = &step.manifest {
        if selection.lane.as_deref() != Some(&manifest.lane)
            || selection.category.as_deref() != Some(&manifest.category)
            || selection.test != manifest.test
            || selection.mode != manifest.mode
            || selection.backend != manifest.backend
        {
            return Err(format!(
                "retry bounds: {} command disagrees with its declared manifest selector",
                step.tag()
            ));
        }
    } else if step.tag() != "quick.e2e_verify" {
        return Err(format!(
            "retry bounds: manifest node {} lacks its execution selector",
            step.tag()
        ));
    }
    Ok((selection, prebuilt))
}

fn step_timeout_multipliers(
    step: &Step,
    inherited: TimeoutMultipliers,
) -> Result<TimeoutMultipliers, String> {
    Ok(TimeoutMultipliers {
        cpu: match step.env.get(TEST_CPU_TIMEOUT_MULTIPLIER_ENV) {
            Some(value) => parse_timeout_multiplier(Some(value), TEST_CPU_TIMEOUT_MULTIPLIER_ENV)?,
            None => inherited.cpu,
        },
        wall: match step.env.get(TEST_WALL_TIMEOUT_MULTIPLIER_ENV) {
            Some(value) => parse_timeout_multiplier(Some(value), TEST_WALL_TIMEOUT_MULTIPLIER_ENV)?,
            None => inherited.wall,
        },
    })
}

fn require_manifest_selection_headroom(
    manifests: &ManifestSet,
    tag: &str,
    node_timeout_seconds: i64,
    selection: &Selection,
    timeout_multipliers: TimeoutMultipliers,
    prebuilt: bool,
    attempts: u64,
    termination_grace_seconds: u64,
) -> Result<Option<i64>, String> {
    let cells = manifests
        .select(selection)
        .map_err(|error| format!("retry bounds: cannot select cells for {tag}: {error}"))?;
    let mut largest_wall_seconds = None;
    for cell in &cells {
        let resolved = resolved_cell_timeouts(cell, timeout_multipliers).map_err(|error| {
            format!(
                "retry bounds: cannot resolve effective timeouts for {tag} cell {:?}: {error}",
                cell.id
            )
        })?;
        largest_wall_seconds = Some(
            largest_wall_seconds.map_or(resolved.wall_seconds, |current: u64| {
                current.max(resolved.wall_seconds)
            }),
        );
    }
    let Some(largest_wall_seconds) = largest_wall_seconds else {
        return Ok(None);
    };
    require_resolved_outer_timeout_headroom(
        tag,
        node_timeout_seconds,
        largest_wall_seconds,
        if prebuilt { 1 } else { 2 },
        termination_grace_seconds,
        attempts,
        &format!(
            "after CPU multiplier {} and wall multiplier {} in {} mode",
            timeout_multipliers.cpu,
            timeout_multipliers.wall,
            if prebuilt { "prebuilt" } else { "non-prebuilt" }
        ),
    )
    .map(Some)
}

#[cfg(test)]
mod nextest_timeout_tests {
    use super::*;

    #[test]
    fn nextest_and_manifest_share_base_and_scaled_wall_bounds() {
        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .expect("validate.rs has a repository parent")
            .to_path_buf();
        let config = std::fs::read_to_string(root.join(".config/nextest.toml")).unwrap();
        let manifest =
            std::fs::read_to_string(root.join("tests/e2e/manifests/defaults.yaml")).unwrap();
        let base_caps = parse_nextest_timeout_caps(&config).unwrap();
        assert_eq!(
            base_caps,
            vec![NextestTimeoutCap {
                period_seconds: DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
                terminate_after: 1,
                grace_seconds: 2,
            }]
        );
        assert!(manifest.lines().any(|line| line == "timeout_seconds: 57"));
        assert!(manifest
            .lines()
            .any(|line| line == "cpu_timeout_seconds: 22"));

        let multiplier = 1.25;
        let scaled = render_scaled_nextest_config(&root, multiplier).unwrap();
        let scaled_caps = parse_nextest_timeout_caps(&scaled).unwrap();
        require_matching_scaled_default(
            DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
            multiplier,
            &scaled_caps,
        )
        .unwrap();

        assert!(
            require_matching_scaled_default(56, multiplier, &scaled_caps).is_err(),
            "a planted committed-base mismatch escaped the consistency gate"
        );
        assert!(
            require_matching_scaled_default(DEFAULT_TEST_WALL_TIMEOUT_SECONDS, 1.20, &scaled_caps,)
                .is_err(),
            "a planted multiplier drift escaped the consistency gate"
        );
    }

    #[test]
    fn enclosing_timeout_checks_scale_and_refuse_unsafe_multipliers() {
        assert_eq!(
            require_outer_timeout_headroom("fixture", 600, 74, 10, 2, 1.0).unwrap(),
            432
        );
        assert_eq!(
            require_outer_timeout_headroom("fixture", 600, 74, 10, 2, 1.5).unwrap(),
            358
        );
        let error = require_outer_timeout_headroom("fixture", 600, 118, 10, 2, 10.0)
            .expect_err("an oversized wall multiplier must not outgrow the outer backup");
        assert!(error.contains("wall multiplier 10"), "{error}");
        assert!(error.contains("can consume 2380s"), "{error}");
    }

    #[test]
    fn production_manifest_headroom_uses_effective_bounds_and_refuses_invalid() {
        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .expect("validate.rs has a repository parent")
            .to_path_buf();
        let quick = validate_plan::validation_config(&root)
            .unwrap()
            .steps
            .into_iter()
            .find(|step| step.tag() == "quick.e2e_verify")
            .expect("the committed quick manifest node is present");
        let manifests = ManifestSet::load(&root).unwrap();
        assert_eq!(
            manifests
                .select(&Selection {
                    population: Some(Population::Required),
                    ..Default::default()
                })
                .unwrap()
                .len(),
            712,
            "timeout accounting must not change the shipped required-cell population"
        );
        let selection = Selection {
            population: Some(Population::Required),
            lane: Some("portable".into()),
            test: Some("backend-parity-c/readdir-order-identity".into()),
            mode: Some("verify".into()),
            backend: Some("ptrace".into()),
            ..Default::default()
        };
        let cells = manifests.select(&selection).unwrap();
        assert_eq!(
            cells.len(),
            1,
            "the shipped production cell must stay selected"
        );
        let cell = &cells[0];
        let multipliers = TimeoutMultipliers {
            cpu: 1.25,
            wall: 1.5,
        };
        let resolved = resolved_cell_timeouts(cell, multipliers).unwrap();
        assert_eq!(resolved.cpu_seconds, 28);
        assert_eq!(resolved.wall_seconds, 86);
        assert!(!manifest_command_is_prebuilt("quick.e2e_verify", &quick.cmd).unwrap());
        assert_eq!(
            require_manifest_selection_headroom(
                &manifests,
                "quick.e2e_verify",
                quick.timeout,
                &selection,
                multipliers,
                false,
                validate_runtime::MAX_ATTEMPTS_PER_CELL as u64,
                10,
            )
            .unwrap(),
            Some(1436)
        );
        assert_eq!(
            require_manifest_selection_headroom(
                &manifests,
                "prebuilt-control",
                quick.timeout,
                &selection,
                multipliers,
                true,
                validate_runtime::MAX_ATTEMPTS_PER_CELL as u64,
                10,
            )
            .unwrap(),
            Some(1608),
            "prebuilt cells have no separate fixture-preparation wall window"
        );

        for (invalid, expected) in [
            (
                TimeoutMultipliers {
                    cpu: f64::NAN,
                    wall: 1.0,
                },
                "must be finite and greater than zero",
            ),
            (
                TimeoutMultipliers {
                    cpu: 3.0,
                    wall: 1.0,
                },
                "scaled wall timeout must remain greater",
            ),
        ] {
            let error = require_manifest_selection_headroom(
                &manifests,
                "quick.e2e_verify",
                quick.timeout,
                &selection,
                invalid,
                false,
                validate_runtime::MAX_ATTEMPTS_PER_CELL as u64,
                10,
            )
            .expect_err("invalid effective timeout policy must be refused");
            assert!(error.contains(expected), "{error}");
        }

        let zstd = Selection {
            population: Some(Population::Required),
            lane: Some("portable".into()),
            test: Some("data-handling/zstd-multithread".into()),
            mode: Some("verify".into()),
            backend: Some("ptrace".into()),
            ..Default::default()
        };
        let slow_host = TimeoutMultipliers {
            cpu: 1.0,
            wall: 4.0,
        };
        let error = require_manifest_selection_headroom(
            &manifests,
            "quick.e2e_verify",
            quick.timeout,
            &zstd,
            slow_host,
            false,
            validate_runtime::MAX_ATTEMPTS_PER_CELL as u64,
            10,
        )
        .expect_err("non-prebuilt zstd can outgrow the quick node at wall 4x");
        assert!(error.contains("can consume 1908s"), "{error}");
        assert_eq!(
            require_manifest_selection_headroom(
                &manifests,
                "prebuilt-zstd-control",
                quick.timeout,
                &zstd,
                slow_host,
                true,
                validate_runtime::MAX_ATTEMPTS_PER_CELL as u64,
                10,
            )
            .unwrap(),
            Some(836),
            "prebuilt zstd must not be charged for fixture preparation it skips"
        );
        assert!(manifest_command_is_prebuilt(
            "prebuilt-control",
            &format!(
                "{} --prebuilt",
                manifest_command_source("quick.e2e_verify", &quick.cmd).unwrap()
            )
        )
        .unwrap());
        assert!(manifest_command_is_prebuilt(
            "duplicate-control",
            &format!(
                "{} --prebuilt --prebuilt",
                manifest_command_source("quick.e2e_verify", &quick.cmd).unwrap()
            )
        )
        .is_err());
    }

    #[test]
    fn split_validate_forwards_distinct_timeout_settings_to_the_shared_policy() {
        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .expect("validate.rs has a repository parent")
            .to_path_buf();
        let scratch = tempfile::tempdir().unwrap();
        let bin = scratch.path().join("bin");
        let out = scratch.path().join("out");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(out.join("cargo/registry")).unwrap();
        let podman = bin.join("podman");
        std::fs::write(
            &podman,
            r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == image && ${2:-} == exists ]]; then
    exit 0
fi
[[ ${1:-} == run ]] || exit 90
cpu_seen=0
wall_seen=0
cpu_value=
wall_value=
while (($#)); do
    if [[ $1 == --env ]]; then
        name=$2
        case "$name" in
            HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER)
                cpu_seen=$((cpu_seen + 1))
                cpu_value=${!name}
                ;;
            HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER)
                wall_seen=$((wall_seen + 1))
                wall_value=${!name}
                ;;
        esac
        shift 2
    else
        shift
    fi
done
[[ $cpu_seen -eq 1 && $wall_seen -eq 1 ]] || exit 91
printf 'FORWARDED_CPU=%s\nFORWARDED_WALL=%s\n' "$cpu_value" "$wall_value"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&podman).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&podman, permissions).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var_os("PATH")
                .unwrap_or_default()
                .to_string_lossy()
        );
        let output = Command::new(root.join("ci/hermetic/run-split-validate.sh"))
            .args([
                "--offline-only",
                "--shards",
                "unit",
                "--out",
                out.to_str().unwrap(),
            ])
            .env("PATH", path)
            .env(TEST_CPU_TIMEOUT_MULTIPLIER_ENV, "1.25")
            .env(TEST_WALL_TIMEOUT_MULTIPLIER_ENV, "1.75")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "split wrapper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("FORWARDED_CPU=1.25"), "{stdout}");
        assert!(stdout.contains("FORWARDED_WALL=1.75"), "{stdout}");

        let multipliers = TimeoutMultipliers {
            cpu: parse_timeout_multiplier(Some("1.25"), TEST_CPU_TIMEOUT_MULTIPLIER_ENV).unwrap(),
            wall: parse_timeout_multiplier(Some("1.75"), TEST_WALL_TIMEOUT_MULTIPLIER_ENV).unwrap(),
        };
        assert_eq!(multipliers.cpu, 1.25);
        assert_eq!(multipliers.wall, 1.75);
        assert_eq!(
            resolve_test_timeouts(
                DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
                DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
                multipliers,
            )
            .unwrap(),
            hermit_manifest_plan::timeouts::ResolvedTestTimeouts {
                cpu_seconds: 28,
                wall_seconds: 100,
            }
        );
        for (name, value) in [
            (TEST_CPU_TIMEOUT_MULTIPLIER_ENV, "malformed"),
            (TEST_WALL_TIMEOUT_MULTIPLIER_ENV, "0"),
        ] {
            let error = parse_timeout_multiplier(Some(value), name)
                .expect_err("the shared timeout policy must refuse malformed input");
            assert!(error.contains(name), "{error}");
        }
    }
    #[test]
    fn committed_manifest_commands_preserve_every_selected_identity_and_preparation_mode() {
        let root = Path::new(file!()).parent().unwrap().parent().unwrap();
        let cfg = validate_plan::validation_config(root).unwrap();
        let manifests = ManifestSet::load(root).unwrap();
        let steps = cfg
            .steps
            .iter()
            .filter(|step| step.cmd.contains("target/debug/test-harness run "))
            .collect::<Vec<_>>();
        assert_eq!(steps.len(), 33);
        for step in steps {
            let (selection, prebuilt) = manifest_step_policy(step).unwrap();
            assert_eq!(prebuilt, step.tag() != "quick.e2e_verify", "{}", step.tag());
            let selected = manifests
                .select(&selection)
                .unwrap()
                .into_iter()
                .map(|cell| {
                    (
                        selection.lane.clone().unwrap(),
                        cell.category,
                        cell.id.test,
                        cell.id.mode,
                        cell.id.backend,
                    )
                })
                .collect::<BTreeSet<_>>();
            let declared = step
                .result_manifests
                .iter()
                .flatten()
                .filter_map(|result| match result {
                    ResultManifest::ManifestCell(cell) => Some((
                        cell.lane.clone(),
                        cell.category.clone(),
                        cell.test.clone().unwrap(),
                        cell.mode.clone().unwrap(),
                        cell.backend.clone(),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                declared.len(),
                selected.len(),
                "{} has duplicated or missing ownership",
                step.tag()
            );
            assert_eq!(
                declared.into_iter().collect::<BTreeSet<_>>(),
                selected,
                "{}",
                step.tag()
            );
        }
        let base = "target/debug/test-harness run --lane portable --ci-only";
        assert!(!manifest_command_is_prebuilt(
            "quoted-path",
            &format!("{base} --results '--prebuilt' --junit 'a b'")
        )
        .unwrap());
        assert!(
            manifest_command_is_prebuilt("quoted-option", &format!("{base} '--prebuilt'")).unwrap()
        );
        assert!(
            manifest_command_is_prebuilt("unknown-option", &format!("{base} --mystery")).is_err()
        );
        let mut step = cfg
            .steps
            .iter()
            .find(|step| step.tag() == "e2e.manifest_data_handling")
            .unwrap()
            .clone();
        step.manifest.as_mut().unwrap().category = "applications".into();
        assert!(manifest_step_policy(&step)
            .unwrap_err()
            .contains("disagrees"));
        step.env
            .insert(TEST_CPU_TIMEOUT_MULTIPLIER_ENV.into(), "1.25".into());
        step.env
            .insert(TEST_WALL_TIMEOUT_MULTIPLIER_ENV.into(), "1.75".into());
        assert_eq!(
            step_timeout_multipliers(
                &step,
                TimeoutMultipliers {
                    cpu: 9.0,
                    wall: 11.0
                }
            )
            .unwrap(),
            TimeoutMultipliers {
                cpu: 1.25,
                wall: 1.75
            }
        );
    }

    #[test]
    fn resolved_headroom_refuses_zero_and_overflow_without_relaxing_equality() {
        for (node, inner, windows, grace, attempts) in [
            (600, 1, 0, 0, 2),
            (600, 1, 1, 0, 0),
            (600, 0, 1, 0, 2),
            (0, 1, 1, 0, 2),
            (-1, 1, 1, 0, 2),
            (i64::MAX, u64::MAX, 2, 0, 1),
            (i64::MAX, u64::MAX, 1, 1, 1),
            (i64::MAX, 1, 2, 1, u64::MAX),
            (364, 86, 2, 10, 2),
        ] {
            assert!(require_resolved_outer_timeout_headroom(
                "invalid-control",
                node,
                inner,
                windows,
                grace,
                attempts,
                "fixture"
            )
            .is_err());
        }
        assert_eq!(
            require_resolved_outer_timeout_headroom(
                "one-second-control",
                365,
                86,
                2,
                10,
                2,
                "fixture"
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn timeout_policy_subprocess_reads_the_forwarded_environment() {
        if std::env::var("HERMIT_VALIDATE_TIMEOUT_POLICY_CHILD").as_deref() != Ok("1") {
            return;
        }
        let result = (|| -> Result<_, String> {
            let multipliers = timeout_multipliers_from_env()?;
            let resolved = resolve_test_timeouts(
                DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
                DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
                multipliers,
            )?;
            let root = Path::new(file!()).parent().unwrap().parent().unwrap();
            let manifests = ManifestSet::load(root)?;
            let cfg = validate_plan::validation_config(root)?;
            let quick = cfg
                .steps
                .iter()
                .find(|step| step.tag() == "quick.e2e_verify")
                .ok_or("quick node missing")?;
            let (selection, prebuilt) = manifest_step_policy(quick)?;
            require_manifest_selection_headroom(
                &manifests,
                &quick.tag(),
                quick.timeout,
                &selection,
                multipliers,
                prebuilt,
                validate_runtime::MAX_ATTEMPTS_PER_CELL as u64,
                10,
            )?;
            Ok(resolved)
        })();
        match result {
            Ok(resolved) => println!(
                "RESOLVED_CPU={} RESOLVED_WALL={}",
                resolved.cpu_seconds, resolved.wall_seconds
            ),
            Err(error) => {
                eprintln!("TIMEOUT_POLICY_REFUSED: {error}");
                std::process::exit(2);
            }
        }
    }

    #[test]
    fn actual_pinned_root_paths_preserve_or_refuse_timeout_policy() {
        let root = Path::new(file!()).parent().unwrap().parent().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let bin = scratch.path().join("bin");
        let out = scratch.path().join("out");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(out.join("cargo/registry")).unwrap();
        let podman = bin.join("podman");
        std::fs::write(&podman, r#"#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == image && ${2:-} == exists ]]; then exit 0; fi
[[ ${1:-} == run ]] || exit 90
cpu_seen=0; wall_seen=0; forwarded=()
while (($#)); do
    if [[ $1 == --env ]]; then
        name=$2
        case "$name" in
            HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER)
                cpu_seen=$((cpu_seen + 1)); forwarded+=("$name=${!name}");;
            HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER)
                wall_seen=$((wall_seen + 1)); forwarded+=("$name=${!name}");;
        esac
        shift 2
    else shift; fi
done
[[ $cpu_seen -eq $EXPECTED_CPU_FORWARD_COUNT && $wall_seen -eq $EXPECTED_WALL_FORWARD_COUNT ]] || {
    echo "MISSING_OR_DUPLICATED_TIMEOUT_PROPAGATION cpu=$cpu_seen wall=$wall_seen" >&2; exit 91;
}
exec env -u HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER -u HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER \
    "${forwarded[@]}" "$TIMEOUT_POLICY_TEST_EXE" \
    --exact nextest_timeout_tests::timeout_policy_subprocess_reads_the_forwarded_environment --nocapture
"#).unwrap();
        std::fs::set_permissions(&podman, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = validate_plan::validation_config(root).unwrap();
        let quick = cfg
            .steps
            .iter()
            .find(|step| step.tag() == "quick.e2e_verify")
            .unwrap();
        let old_paths =
            "--out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo";
        assert_eq!(quick.cmd.matches(old_paths).count(), 1);
        let command = quick.cmd.replace(
            old_paths,
            &format!(
                "--out {} --src-rw --cargo-home {}",
                validate_plan::shell_quote(&out.to_string_lossy()),
                validate_plan::shell_quote(&out.join("cargo").to_string_lossy())
            ),
        );
        let split_args = vec![
            root.join("ci/hermetic/run-split-validate.sh")
                .to_string_lossy()
                .into_owned(),
            "--offline-only".into(),
            "--shards".into(),
            "unit".into(),
            "--out".into(),
            out.to_string_lossy().into_owned(),
        ];
        let paths = [
            split_args.clone(),
            vec!["bash".into(), "-c".into(), command.clone()],
        ];
        let run = |argv: &[String], cpu: Option<&str>, wall: Option<&str>| {
            let mut child = Command::new(&argv[0]);
            child
                .args(&argv[1..])
                .current_dir(root)
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        bin.display(),
                        std::env::var_os("PATH")
                            .unwrap_or_default()
                            .to_string_lossy()
                    ),
                )
                .env("TIMEOUT_POLICY_TEST_EXE", std::env::current_exe().unwrap())
                .env("HERMIT_VALIDATE_TIMEOUT_POLICY_CHILD", "1")
                .env(
                    "EXPECTED_CPU_FORWARD_COUNT",
                    if cpu.is_some() { "1" } else { "0" },
                )
                .env(
                    "EXPECTED_WALL_FORWARD_COUNT",
                    if wall.is_some() { "1" } else { "0" },
                )
                .env_remove(TEST_CPU_TIMEOUT_MULTIPLIER_ENV)
                .env_remove(TEST_WALL_TIMEOUT_MULTIPLIER_ENV);
            if let Some(value) = cpu {
                child.env(TEST_CPU_TIMEOUT_MULTIPLIER_ENV, value);
            }
            if let Some(value) = wall {
                child.env(TEST_WALL_TIMEOUT_MULTIPLIER_ENV, value);
            }
            child.output().unwrap()
        };
        for argv in paths {
            for (cpu, wall, expected) in [
                (
                    Some("1.25"),
                    Some("1.75"),
                    Ok("RESOLVED_CPU=28 RESOLVED_WALL=100"),
                ),
                (None, None, Ok("RESOLVED_CPU=22 RESOLVED_WALL=57")),
                (
                    Some("malformed"),
                    Some("1.75"),
                    Err(TEST_CPU_TIMEOUT_MULTIPLIER_ENV),
                ),
                (
                    Some("1.25"),
                    Some("0"),
                    Err(TEST_WALL_TIMEOUT_MULTIPLIER_ENV),
                ),
                (
                    Some("3"),
                    Some("1"),
                    Err("scaled wall timeout must remain greater"),
                ),
                (Some("1e308"), Some("1e308"), Err("overflows")),
                (Some("1"), Some("4"), Err("can consume 1908s")),
            ] {
                let output = run(&argv, cpu, wall);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                match expected {
                    Ok(marker) => {
                        assert!(output.status.success(), "{argv:?}: {stderr}");
                        assert!(stdout.contains(marker), "{stdout}");
                    }
                    Err(marker) => {
                        assert_eq!(output.status.code(), Some(2), "{argv:?}: {stdout} {stderr}");
                        assert!(stderr.contains(marker), "{stderr}");
                    }
                }
            }
        }
        for name in [
            TEST_CPU_TIMEOUT_MULTIPLIER_ENV,
            TEST_WALL_TIMEOUT_MULTIPLIER_ENV,
        ] {
            let argument = format!("--env {name} ");
            assert_eq!(command.matches(&argument).count(), 1);
            let broken = vec!["bash".into(), "-c".into(), command.replace(&argument, "")];
            let output = run(&broken, Some("1.25"), Some("1.75"));
            assert_eq!(output.status.code(), Some(91));
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("MISSING_OR_DUPLICATED_TIMEOUT_PROPAGATION"));
        }
        let copied_root = scratch.path().join("split-mutation");
        let copied_scripts = copied_root.join("ci/hermetic");
        std::fs::create_dir_all(&copied_scripts).unwrap();
        for name in [
            "run-split-validate.sh",
            "run-in-pinned-root.sh",
            "image.digest",
        ] {
            std::fs::copy(
                root.join("ci/hermetic").join(name),
                copied_scripts.join(name),
            )
            .unwrap();
        }
        for name in ["portable-shards.json", "expected-e2e-plan.json"] {
            std::fs::copy(
                root.join("ci").join(name),
                copied_root.join("ci").join(name),
            )
            .unwrap();
        }
        let source = std::fs::read_to_string(copied_scripts.join("run-split-validate.sh")).unwrap();
        for name in [
            TEST_CPU_TIMEOUT_MULTIPLIER_ENV,
            TEST_WALL_TIMEOUT_MULTIPLIER_ENV,
        ] {
            let argument = format!("        --env {name} \\\n");
            assert_eq!(source.matches(&argument).count(), 1);
            std::fs::write(
                copied_scripts.join("run-split-validate.sh"),
                source.replace(&argument, ""),
            )
            .unwrap();
            let mut broken = split_args.clone();
            broken[0] = copied_scripts
                .join("run-split-validate.sh")
                .to_string_lossy()
                .into_owned();
            let output = run(&broken, Some("1.25"), Some("1.75"));
            assert_eq!(
                output.status.code(),
                Some(91),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("MISSING_OR_DUPLICATED_TIMEOUT_PROPAGATION"));
        }
    }
}

fn unreported_non_intentional_steps(
    cfg: &DagConfig,
    by_tag: &BTreeMap<String, StepOutcome>,
    skipped: &[String],
) -> Vec<String> {
    let skipped: BTreeSet<&str> = skipped.iter().map(String::as_str).collect();
    cfg.steps
        .iter()
        .filter(|step| {
            let tag = step.tag();
            step.skip_reason.is_none()
                && !by_tag.contains_key(&tag)
                && !skipped.contains(tag.as_str())
        })
        .map(|step| step.tag())
        .collect()
}

fn retry_timeout_bound_bracket(root: &Path) -> Result<String, String> {
    const DEFAULT_TEST_CAP_S: i64 = DEFAULT_TEST_WALL_TIMEOUT_SECONDS as i64;
    const NEXTEST_TERMINATION_GRACE_S: i64 = 2;
    const MANIFEST_TERMINATION_GRACE_S: i64 = 10;

    fn require_live_nextest_output(tag: &str, command: &str) -> Result<(), String> {
        let Some(wrapper) = command.find("run-nextest-counted.sh") else {
            return Ok(());
        };
        let invocation = &command[wrapper..];
        if invocation.contains(">\"$log\" 2>&1")
            || invocation.contains("cat \"$log\"")
            || invocation.contains("sed -n 's/^running ")
        {
            return Err(format!(
                "retry bounds: {tag} buffers or reparses run-nextest-counted output; test events \
                 must remain live and exact counts must be checked by the wrapper"
            ));
        }
        Ok(())
    }

    fn require_expected_nextest_count(
        tag: &str,
        environment: &BTreeMap<String, String>,
        expected: usize,
    ) -> Result<(), String> {
        match environment.get("NEXTEST_EXPECTED_EXECUTED") {
            Some(actual) if actual == &expected.to_string() => Ok(()),
            Some(actual) => Err(format!(
                "retry bounds: {tag} must require exactly {expected} executed tests through its typed environment, got {actual:?}"
            )),
            None => Err(format!(
                "retry bounds: {tag} must require exactly {expected} executed tests through its typed environment"
            )),
        }
    }

    let nextest = std::fs::read_to_string(root.join(".config/nextest.toml"))
        .map_err(|e| format!("retry bounds: cannot read nextest config: {e}"))?;
    let nextest_wrapper = std::fs::read_to_string(root.join("ci/run-nextest-counted.sh"))
        .map_err(|e| format!("retry bounds: cannot read nextest wrapper: {e}"))?;
    if !nextest_wrapper.contains("${HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER-1}") {
        return Err(format!(
            "retry bounds: nextest wrapper does not read {TEST_WALL_TIMEOUT_MULTIPLIER_ENV} with an unset-only identity default"
        ));
    }
    let manifest_defaults = std::fs::read_to_string(root.join("tests/e2e/manifests/defaults.yaml"))
        .map_err(|e| format!("retry bounds: cannot read manifest defaults: {e}"))?;
    if !manifest_defaults
        .lines()
        .any(|line| line == "timeout_seconds: 57")
    {
        return Err(
            "retry bounds: the owner-ruled 57-second wall default is absent from manifest defaults"
                .into(),
        );
    }
    let base_nextest_caps = parse_nextest_timeout_caps(&nextest)?;
    if base_nextest_caps.len() != 1
        || base_nextest_caps[0].period_seconds != DEFAULT_TEST_WALL_TIMEOUT_SECONDS
        || base_nextest_caps[0].terminate_after != 1
        || base_nextest_caps[0].grace_seconds != NEXTEST_TERMINATION_GRACE_S as u64
    {
        return Err(format!(
            "retry bounds: nextest must carry exactly the canonical {DEFAULT_TEST_CAP_S}s wall cap with one termination period and {NEXTEST_TERMINATION_GRACE_S}s grace, got {base_nextest_caps:?}"
        ));
    }
    let largest_base_nextest_cap_s = base_nextest_caps
        .iter()
        .map(|cap| cap.period_seconds)
        .max()
        .ok_or("retry bounds: no declared base nextest cap")?;
    // Validate both exact machine settings consumed by the manifest runner.
    // Nextest currently consumes only the wall member of this same pair.
    let timeout_multipliers = timeout_multipliers_from_env()?;
    let validated_wall_multiplier = timeout_multipliers.wall;
    let scaled_nextest = render_scaled_nextest_config(root, validated_wall_multiplier)?;
    let scaled_nextest_caps = parse_nextest_timeout_caps(&scaled_nextest)?;
    require_matching_scaled_default(
        DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
        validated_wall_multiplier,
        &scaled_nextest_caps,
    )?;
    let nextest_caps = scaled_nextest_caps
        .iter()
        .map(|cap| {
            if cap.terminate_after != 1 {
                return Err(format!(
                    "retry bounds: generated nextest slow-timeout is not a one-period cap: {cap:?}"
                ));
            }
            if cap.grace_seconds != NEXTEST_TERMINATION_GRACE_S as u64 {
                return Err(format!(
                    "retry bounds: expected {NEXTEST_TERMINATION_GRACE_S}s termination grace, got {}s",
                    cap.grace_seconds
                ));
            }
            i64::try_from(cap.period_seconds)
                .map_err(|error| format!("retry bounds: nextest period is too large: {error}"))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let largest_nextest_cap_s = nextest_caps
        .iter()
        .copied()
        .max()
        .ok_or("retry bounds: no declared nextest cap")?;

    let lane_configs = ["portable", "privileged"]
        .into_iter()
        .map(|lane| validate_plan::lane_config(root, lane).map(|cfg| (lane, cfg)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut streamed_nextest_nodes = 0usize;
    let mut tightest_nextest_headroom_s = i64::MAX;
    for (_, cfg) in &lane_configs {
        for step in cfg
            .steps
            .iter()
            .filter(|step| step.cmd.contains("run-nextest-counted.sh"))
        {
            require_live_nextest_output(&step.tag(), &step.cmd)?;
            let actual_headroom = require_outer_timeout_headroom(
                &step.tag(),
                step.timeout,
                largest_base_nextest_cap_s,
                NEXTEST_TERMINATION_GRACE_S as u64,
                1,
                validated_wall_multiplier,
            )?;
            // A 1.5x slow-host factor is the representative non-identity
            // control. The same helper below receives the actual configured
            // factor once its external binding is read.
            require_outer_timeout_headroom(
                &step.tag(),
                step.timeout,
                DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
                NEXTEST_TERMINATION_GRACE_S as u64,
                1,
                1.5,
            )?;
            tightest_nextest_headroom_s = tightest_nextest_headroom_s.min(actual_headroom);
            streamed_nextest_nodes += 1;
        }
    }
    let privileged = lane_configs
        .iter()
        .find(|(lane, _)| *lane == "privileged")
        .map(|(_, cfg)| cfg)
        .ok_or("retry bounds: privileged lane is absent")?;
    for (tag, expected) in [
        ("privileged-only-test.pmu_buck_chaos_cases", 6usize),
        ("privileged-only-test.cli_kvm", 24usize),
    ] {
        let step = privileged
            .steps
            .iter()
            .find(|step| step.tag() == tag)
            .ok_or_else(|| format!("retry bounds: exact-count node {tag} is absent"))?;
        require_expected_nextest_count(tag, &step.env, expected)?;
    }
    let buffered_mutation =
        "./ci/run-nextest-counted.sh -p fixture >\"$log\" 2>&1 || status=$?; cat \"$log\"";
    let buffered_error = require_live_nextest_output("test.fixture", buffered_mutation)
        .expect_err("buffered nextest output mutation must be refused");
    if !buffered_error.contains("test.fixture") || !buffered_error.contains("must remain live") {
        return Err(format!(
            "retry bounds: buffered-output mutation did not fail by node name: {buffered_error}"
        ));
    }
    let count_error =
        require_expected_nextest_count("test.fixture", &BTreeMap::new(), 7)
            .expect_err("missing exact-count declaration mutation must be refused");
    if !count_error.contains("test.fixture") || !count_error.contains("exactly 7") {
        return Err(format!(
            "retry bounds: exact-count mutation did not fail by node name: {count_error}"
        ));
    }
    let attempts = validate_runtime::MAX_ATTEMPTS_PER_CELL as u64;
    let default_with_grace_s = i64::try_from(scale_timeout_seconds(
        DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
        validated_wall_multiplier,
        "validated wall multiplier",
    )?)
    .map_err(|error| format!("retry bounds: scaled default is too large: {error}"))?
        + NEXTEST_TERMINATION_GRACE_S;
    let largest_nextest_with_grace_s = largest_nextest_cap_s + NEXTEST_TERMINATION_GRACE_S;

    let committed = validate_plan::validation_config(root)?;
    let quick = committed.steps.iter().find(|step| step.tag() == "quick.e2e_verify")
        .ok_or("retry bounds: committed quick manifest node is absent")?;
    let manifests = ManifestSet::load(root)
        .map_err(|e| format!("retry bounds: cannot load E2E manifests: {e}"))?;
    let mut checked_manifest_nodes = 0usize;
    let mut tightest_manifest_headroom_s = i64::MAX;
    let representative_multipliers = TimeoutMultipliers {
        cpu: 1.25,
        wall: 1.5,
    };
    for step in committed.steps.iter()
        .filter(|step| step.cmd.contains("target/debug/test-harness run "))
    {
        let (selection, prebuilt) = manifest_step_policy(step)?;
        let effective_multipliers = step_timeout_multipliers(step, timeout_multipliers)?;
        if let Some(headroom_s) = require_manifest_selection_headroom(
            &manifests, &step.tag(), step.timeout, &selection,
            effective_multipliers, prebuilt, attempts,
            MANIFEST_TERMINATION_GRACE_S as u64,
        )? {
            checked_manifest_nodes += 1;
            tightest_manifest_headroom_s = tightest_manifest_headroom_s.min(headroom_s);
        }
        require_manifest_selection_headroom(
            &manifests, &step.tag(), step.timeout, &selection,
            representative_multipliers, prebuilt, attempts,
            MANIFEST_TERMINATION_GRACE_S as u64,
        )?;
    }
    let (quick_selection, quick_prebuilt) = manifest_step_policy(quick)?;
    let invalid_multiplier_error = require_manifest_selection_headroom(
        &manifests,
        "invalid-multiplier-control",
        quick.timeout,
        &quick_selection,
        TimeoutMultipliers {
            cpu: f64::NAN,
            wall: 1.0,
        },
        quick_prebuilt,
        attempts,
        MANIFEST_TERMINATION_GRACE_S as u64,
    )
    .expect_err("a non-finite CPU multiplier must be refused by the production adapter");
    if !invalid_multiplier_error.contains("must be finite and greater than zero") {
        return Err(format!(
            "retry bounds: invalid-multiplier control failed for the wrong reason: {invalid_multiplier_error}"
        ));
    }
    let inverted_policy_error = require_manifest_selection_headroom(
        &manifests,
        "inverted-policy-control",
        quick.timeout,
        &quick_selection,
        TimeoutMultipliers {
            cpu: 3.0,
            wall: 1.0,
        },
        quick_prebuilt,
        attempts,
        MANIFEST_TERMINATION_GRACE_S as u64,
    )
    .expect_err("scaled CPU >= wall must be refused by the production adapter");
    if !inverted_policy_error.contains("scaled wall timeout must remain greater") {
        return Err(format!(
            "retry bounds: inverted-policy control failed for the wrong reason: {inverted_policy_error}"
        ));
    }
    let oversized_error = require_manifest_selection_headroom(
        &manifests,
        "oversized-multiplier-control",
        quick.timeout,
        &quick_selection,
        TimeoutMultipliers {
            cpu: 1.0,
            wall: 10.0,
        },
        quick_prebuilt,
        attempts,
        MANIFEST_TERMINATION_GRACE_S as u64,
    )
    .expect_err("an oversized multiplier must be refused by the enclosing bound gate");
    if !oversized_error.contains("wall multiplier 10") {
        return Err(format!(
            "retry bounds: oversized-multiplier control failed for the wrong reason: {oversized_error}"
        ));
    }
    let zstd_selection = Selection {
        test: Some("data-handling/zstd-multithread".into()),
        ..quick_selection.clone()
    };
    let nonprebuilt_zstd_error = require_manifest_selection_headroom(
        &manifests,
        "nonprebuilt-zstd-control",
        quick.timeout,
        &zstd_selection,
        TimeoutMultipliers {
            cpu: 1.0,
            wall: 4.0,
        },
        quick_prebuilt,
        attempts,
        MANIFEST_TERMINATION_GRACE_S as u64,
    )
    .expect_err("non-prebuilt zstd at wall 4x must exceed the enclosing node bound");
    if !nonprebuilt_zstd_error.contains("wall multiplier 4")
        || !nonprebuilt_zstd_error.contains("can consume 1908s")
    {
        return Err(format!(
            "retry bounds: non-prebuilt zstd control failed for the wrong reason: {nonprebuilt_zstd_error}"
        ));
    }

    // Non-manifest DAG nodes get one outer execution, with their test framework
    // enforcing its own per-test cap. Manifest retries happen inside one node.
    // Each prebuilt attempt gets one execution wall window; each non-prebuilt
    // attempt first gets a separate fixture-preparation wall window. Every
    // attempt's applicable windows and cleanup grace must fit the node timeout.
    if attempts != 2
        || nextest_caps.first().copied() != Some(default_with_grace_s - NEXTEST_TERMINATION_GRACE_S)
        || tightest_nextest_headroom_s == i64::MAX
        || checked_manifest_nodes == 0
    {
        return Err(format!(
            "retry bounds: attempts={attempts}; default nextest cap including grace=\
             {default_with_grace_s}s; largest nextest cap including grace=\
             {largest_nextest_with_grace_s}s; checked manifest nodes=\
             {checked_manifest_nodes}; tightest nextest node headroom=\
             {tightest_nextest_headroom_s}s"
        ));
    }
    Ok(format!(
        "retry bounds: {streamed_nextest_nodes} nextest node(s) keep test events live; \
         outer DAG nodes run once; default nextest cap including grace=\
         {default_with_grace_s}s and largest nextest cap including grace=\
         {largest_nextest_with_grace_s}s leave at least {tightest_nextest_headroom_s}s in every \
         enclosing nextest node; {checked_manifest_nodes} manifest node(s) fit both cell attempts \
         with their production prebuilt/non-prebuilt preparation and execution windows at CPU \
         {}x/wall {}x and representative CPU {}x/wall {}x, leaving at least \
         {tightest_manifest_headroom_s}s at the configured multipliers; invalid, inverted, and \
         oversized policies are refused",
        timeout_multipliers.cpu,
        timeout_multipliers.wall,
        representative_multipliers.cpu,
        representative_multipliers.wall,
    ))
}

/// Fast front-door bracket for the scheduler result shape consumed below.
fn scheduler_accounting_bracket() -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!(
        "validate-scheduler-accounting-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    std::fs::create_dir(&tmp)
        .map_err(|e| format!("scheduler accounting: cannot create {}: {e}", tmp.display()))?;
    let evidence_dir = tmp.join("dagrun-evidence");
    std::fs::create_dir(&evidence_dir).map_err(|e| {
        format!(
            "scheduler accounting: cannot create private evidence directory {}: {e}",
            evidence_dir.display()
        )
    })?;
    std::fs::set_permissions(&evidence_dir, std::fs::Permissions::from_mode(0o700)).map_err(
        |e| {
            format!(
                "scheduler accounting: cannot make evidence directory private {}: {e}",
                evidence_dir.display()
            )
        },
    )?;
    let prior_dagrun_log_dir = std::env::var_os("DAGRUN_LOG_DIR");
    // SAFETY: this self-test runs its scheduler fixtures serially. Restored
    // immediately after the bracket, including when a fixture returns Err.
    unsafe { std::env::set_var("DAGRUN_LOG_DIR", &evidence_dir) };

    let result = (|| -> Result<(), String> {
    let step = |job: &str, cmd: &str| {
        step_with_caps(
            "fixture",
            job,
            "validate scheduler accounting fixture",
            cmd.to_string(),
            Vec::new(),
            30,
            30,
            64 * 1024 * 1024,
        )
    };
    let mut intentional_skip = step("intentional_skip", "exit 99");
    intentional_skip.skip_reason = Some(
        dagrun::model::IntentionalSkipReason::EmptyManifestBucket,
    );

    // A complete runnable plan plus a typed intentional skip is complete and
    // green. The skipped command is `exit 99`, so executing it cannot accidentally
    // satisfy the positive case.
    let complete_cfg = DagConfig {
        steps: vec![step("pass", "true"), intentional_skip.clone()],
        ..Default::default()
    };
    let complete = run_lane_once(
        &complete_cfg,
        1,
        true,
        0,
        None,
        &tmp.join("complete.log"),
        None,
        false,
    );
    if !complete.complete
        || !complete.ok
        || completed_node_count(&complete.outcomes, &complete.attempts) != 1
        || complete.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
            != vec!["fixture.pass"]
    {
        return Err(format!(
            "scheduler accounting: complete plan plus intentional skip was not accepted: complete={} ok={} outcomes={:?}",
            complete.complete,
            complete.ok,
            complete.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
        ));
    }

    // A genuine failure must not be reclassified or retried, and the lane's
    // completeness axis must report whether the rest of the plan was measured.
    //
    // These two runs are the same DAG under the two launch policies, and they are
    // bracketed together because the difference between them is the whole point of
    // the completeness axis. Until agent-utils 6a3c2d7 ("make --keep-going keep
    // going") a `keep_going` run suppressed the eager reap of in-flight steps and
    // then launched nothing further, so BOTH policies left the peers unmeasured and
    // this bracket could not tell them apart. It now checks each one separately.
    let failure_log = tmp.join("unclassified.log");
    std::fs::write(
        &failure_log,
        "[fixture.fail] ----- detail -----\n[fixture.fail] ordinary test failure\n[fixture.fail] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write {}: {e}", failure_log.display()))?;
    let failed_cfg = DagConfig {
        steps: vec![
            step("fail", "exit 1"),
            step("pending_a", "true"),
            step("pending_b", "true"),
        ],
        ..Default::default()
    };
    // Default eager-exit: with one worker both independent peers remain runnable but
    // never launched, so the lane is a red that is ALSO incomplete, and the
    // completeness axis must refuse to let it exit 0.
    let failed = run_lane_once(
        &failed_cfg,
        1,
        false,
        0,
        None,
        &failure_log,
        None,
        false,
    );
    // One failed outer node gets one execution. Its independent peers remain
    // explicitly unlaunched under eager exit, and no second graph can rewrite
    // either fact.
    if failed.complete
        || failed.ok
        || failed.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
            != vec!["fixture.fail"]
        || exit_code_with_execution_completeness(0, failed.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: a failed outer node must run once and remain an incomplete red: complete={} ok={} outcomes={:?}",
            failed.complete,
            failed.ok,
            failed.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
        ));
    }

    // Same failure under keep-going: still a red, and still not green,
    // but now every peer is measured, so the lane is completely accounted for. The
    // failure verdict must not soften just because coverage got wider.
    let kept = run_lane_once(
        &failed_cfg,
        1,
        true,
        0,
        None,
        &failure_log,
        None,
        false,
    );
    let mut kept_tags: Vec<&str> = kept.outcomes.iter().map(|o| o.tag.as_str()).collect();
    kept_tags.sort_unstable();
    if !kept.complete
        || kept.ok
        || kept_tags != vec!["fixture.fail", "fixture.pending_a", "fixture.pending_b"]
    {
        return Err(format!(
            "scheduler accounting: keep-going did not measure every independent peer while keeping the failure red: complete={} ok={} outcomes={kept_tags:?}",
            kept.complete, kept.ok
        ));
    }

    // A dependency skip is named by the scheduler but still did not execute.
    // It cannot satisfy required-node completeness.
    let dependency_log = tmp.join("dependency-failure.log");
    std::fs::write(
        &dependency_log,
        "[fixture.dependency_failure] ----- detail -----\n[fixture.dependency_failure] ordinary test failure\n[fixture.dependency_failure] ----- end detail -----\n",
    )
    .map_err(|e| {
        format!(
            "scheduler accounting: cannot write {}: {e}",
            dependency_log.display()
        )
    })?;
    let dependency_runs = tmp.join("dependency-failure-runs");
    let dependent_ran = tmp.join("dependency-dependent-ran");
    let dependency_failure_cmd = format!(
        "printf 'attempt\\n' >> {runs}; exit 1",
        runs = validate_plan::shell_quote(&dependency_runs.to_string_lossy()),
    );
    let mut dependency_skipped = step(
        "dependency_skipped",
        &format!(
            ": > {}",
            validate_plan::shell_quote(&dependent_ran.to_string_lossy())
        ),
    );
    dependency_skipped.deps = vec!["fixture.dependency_failure".into()];
    let dependency_cfg = DagConfig {
        steps: vec![
            step("dependency_failure", &dependency_failure_cmd),
            dependency_skipped,
        ],
        ..Default::default()
    };
    let dependency_result = run_lane_once(
        &dependency_cfg,
        1,
        true,
        0,
        None,
        &dependency_log,
        None,
        false,
    );
    let dependency_attempt_count = std::fs::read_to_string(&dependency_runs)
        .unwrap_or_default()
        .lines()
        .count();
    let dependency_failures = dependency_result
        .outcomes
        .iter()
        .filter(|outcome| outcome_is_failure(outcome))
        .count();
    let (_named, listing) = blocking_listing(
        &dependency_result.outcomes,
        &BTreeSet::new(),
        dependency_failures,
    );
    let mut summary_detail = vec![format!("{dependency_failures} blocking failure(s){listing}")];
    summary_detail.extend(execution_completeness_details(
        &dependency_result.skipped,
        dependency_result.complete,
    ));
    let mut dependency_summary = RunSummary::new(Verdict::Fail, 1, "self-test", summary_detail);
    dependency_summary.nodes_executed =
        completed_node_count(&dependency_result.outcomes, &dependency_result.attempts);
    dependency_summary.nodes_failed = dependency_failures;
    dependency_summary.nodes_skipped = dependency_result.skipped.len();
    dependency_summary.wall_s = Some(0.0);
    let rendered_dependency_summary =
        run_summary_lines(&dependency_summary, std::time::Instant::now()).join("\n");
    if dependency_result.complete
        || dependency_result.ok
        || dependency_attempt_count != 1
        || dependency_result.skipped != vec!["fixture.dependency_skipped"]
        || dependent_ran.exists()
        || exit_code_with_execution_completeness(0, dependency_result.complete) == 0
        || !rendered_dependency_summary
            .contains("1 blocking failure(s): fixture.dependency_failure")
        || !rendered_dependency_summary
            .contains("1 node(s) never ran because a dependency failed")
        || !rendered_dependency_summary.contains("work made the run incomplete")
        || !rendered_dependency_summary.ends_with("FINAL_VALIDATE_STATUS: FAILED")
    {
        return Err(format!(
            "scheduler accounting: failed prerequisite was rerun or its failure/dependent skip was not preserved in the terminal summary: complete={} ok={} attempts={dependency_attempt_count} skipped={:?} dependent_ran={} summary={rendered_dependency_summary:?}",
            dependency_result.complete,
            dependency_result.ok,
            dependency_result.skipped,
            dependent_ran.exists(),
        ));
    }

    // Eager-exit reports a running peer as aborted. That typed outcome is not a
    // completed required node and must likewise force a nonzero final exit.
    let aborted_log = tmp.join("aborted-peer.log");
    std::fs::write(
        &aborted_log,
        "[fixture.abort_failure] ----- detail -----\n[fixture.abort_failure] ordinary test failure\n[fixture.abort_failure] ----- end detail -----\n",
    )
    .map_err(|e| {
        format!(
            "scheduler accounting: cannot write {}: {e}",
            aborted_log.display()
        )
    })?;
    let aborted_cfg = DagConfig {
        steps: vec![
            step("abort_failure", "sleep 0.1; exit 1"),
            step("aborted_peer", "sleep 5"),
        ],
        ..Default::default()
    };
    let aborted_result = run_lane_once(
        &aborted_cfg,
        2,
        false,
        0,
        None,
        &aborted_log,
        None,
        false,
    );
    let aborted_peer_reported = aborted_result
        .outcomes
        .iter()
        .any(|outcome| outcome.tag == "fixture.aborted_peer" && outcome.aborted);
    if aborted_result.complete
        || aborted_result.ok
        || !aborted_peer_reported
        || exit_code_with_execution_completeness(0, aborted_result.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: aborted required node did not force incomplete execution: complete={} ok={} aborted_peer_reported={aborted_peer_reported} outcomes={:?}",
            aborted_result.complete,
            aborted_result.ok,
            aborted_result
                .outcomes
                .iter()
                .map(|outcome| (outcome.tag.as_str(), outcome.aborted))
                .collect::<Vec<_>>()
        ));
    }

    // Scoped eager-exit keeps BOTH promises in one run: the failing family is still cut short
    // and its true dependent is skipped, while a different family completes. Checking only the
    // independent pass would also accept a blanket keep-going implementation.
    let scoped_log = tmp.join("scoped-eager-exit.log");
    std::fs::write(
        &scoped_log,
        "[fixture.family_failure] ----- detail -----\n[fixture.family_failure] ordinary test failure\n[fixture.family_failure] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write {}: {e}", scoped_log.display()))?;
    let mut family_failure = step("family_failure", "sleep 0.1; exit 1");
    let mut family_peer = step("family_peer", "sleep 5");
    let mut family_dependent = step("family_dependent", "true");
    family_dependent.deps = vec!["fixture.family_failure".into()];
    let marker = tmp.join("independent-family-completed");
    let independent = step(
        "independent_family",
        &format!(
            "sleep 0.3; : > {}",
            validate_plan::shell_quote(&marker.to_string_lossy())
        ),
    );
    for member in [&mut family_failure, &mut family_peer, &mut family_dependent] {
        member.fail_fast_family = Some("fixture.failure-family".into());
    }
    let mut scoped_plan = Plan {
        cfg: DagConfig {
            steps: vec![family_failure, family_peer, family_dependent, independent],
            ..Default::default()
        },
        ..Default::default()
    };
    assign_fail_fast_families(&mut scoped_plan);
    let scoped_families: BTreeMap<String, String> = scoped_plan
        .cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step.fail_fast_family.clone().unwrap_or_default()))
        .collect();
    if scoped_families["fixture.family_peer"] != "fixture.failure-family"
        || scoped_families["fixture.independent_family"] != "fixture.independent_family"
    {
        return Err(format!(
            "scheduler accounting: plan family assignment changed an explicit family or failed to scope an ordinary node by its tag: {scoped_families:?}"
        ));
    }
    let scoped = run_lane_once(
        &scoped_plan.cfg,
        3,
        false,
        0,
        None,
        &scoped_log,
        None,
        false,
    );
    let scoped_by_tag: BTreeMap<&str, &StepOutcome> = scoped
        .outcomes
        .iter()
        .map(|outcome| (outcome.tag.as_str(), outcome))
        .collect();
    let same_family_aborted = scoped_by_tag
        .get("fixture.family_peer")
        .is_some_and(|outcome| outcome.aborted);
    let independent_completed = scoped_by_tag
        .get("fixture.independent_family")
        .is_some_and(|outcome| outcome.ok && !outcome.aborted);
    if scoped.ok
        || scoped.complete
        || !same_family_aborted
        || scoped.skipped != ["fixture.family_dependent".to_string()]
        || !independent_completed
        || !marker.is_file()
    {
        return Err(format!(
            "scheduler accounting: scoped eager-exit did not cancel its own family, skip its dependent, and complete an independent family: complete={} ok={} skipped={:?} outcomes={:?} marker={}",
            scoped.complete,
            scoped.ok,
            scoped.skipped,
            scoped
                .outcomes
                .iter()
                .map(|outcome| (outcome.tag.as_str(), outcome.ok, outcome.aborted))
                .collect::<Vec<_>>(),
            marker.is_file()
        ));
    }

    // A fully reported failing row can be allowed by a profile's existing
    // policy. Completeness must not silently turn every raw node failure into a
    // blocking failure.
    let allowed_log = tmp.join("allowed-failure.log");
    std::fs::write(
        &allowed_log,
        "[fixture.allowed_failure] ----- detail -----\n[fixture.allowed_failure] expected measured failure\n[fixture.allowed_failure] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write {}: {e}", allowed_log.display()))?;
    let allowed_cfg = DagConfig {
        steps: vec![step("allowed_failure", "exit 1"), intentional_skip],
        ..Default::default()
    };
    let allowed = run_lane_once(
        &allowed_cfg,
        1,
        true,
        0,
        None,
        &allowed_log,
        None,
        false,
    );
    if !allowed.complete
        || allowed.ok
        || exit_code_with_execution_completeness(0, allowed.complete) != 0
    {
        return Err(format!(
            "scheduler accounting: complete allowed failure was not kept distinct from incomplete execution: complete={} ok={}",
            allowed.complete, allowed.ok
        ));
    }

    // An environmental signature is recorded but does not launch a second DAG.
    // The dependent is deliberately registered before its prerequisite so this
    // also proves the original graph still honors its edge under keep-going.
    let environmental_log = tmp.join("environmental.log");
    let first_attempt = tmp.join("environmental-first-attempt");
    let edge_ready = tmp.join("edge-ready");
    let environmental_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.environmental] ----- detail -----' \
         '[fixture.environmental] An action was blocked on this server based on a security policy!' \
         '[fixture.environmental] ----- end detail -----' > {log}; exit 1; fi",
        first = validate_plan::shell_quote(&first_attempt.to_string_lossy()),
        log = validate_plan::shell_quote(&environmental_log.to_string_lossy()),
    );
    let mut dependent = step(
        "dependent",
        &format!(
            "test -f {}",
            validate_plan::shell_quote(&edge_ready.to_string_lossy())
        ),
    );
    dependent.deps = vec!["fixture.prerequisite".into()];
    let environmental_cfg = DagConfig {
        steps: vec![
            step("environmental", &environmental_cmd),
            dependent,
            step(
                "prerequisite",
                &format!(
                    ": > {}",
                    validate_plan::shell_quote(&edge_ready.to_string_lossy())
                ),
            ),
        ],
        ..Default::default()
    };
    let retried = run_lane_once(
        &environmental_cfg,
        1,
        true,
        0,
        None,
        &environmental_log,
        None,
        false,
    );
    let retried_tags: BTreeSet<&str> =
        retried.outcomes.iter().map(|outcome| outcome.tag.as_str()).collect();
    let expected_tags: BTreeSet<&str> = [
        "fixture.environmental",
        "fixture.dependent",
        "fixture.prerequisite",
    ]
    .into_iter()
    .collect();
    if !retried.complete
        || retried.ok
        || retried_tags != expected_tags
        || !retried.skipped.is_empty()
        || !edge_ready.is_file()
    {
        return Err(format!(
            "scheduler accounting: one-shot environmental failure did not preserve its red verdict and original dependency execution: complete={} ok={} outcomes={retried_tags:?} skipped={:?} edge_ready={}",
            retried.complete,
            retried.ok,
            retried.skipped,
            edge_ready.is_file()
        ));
    }

    // The one attempt remains a raw failure. Its environmental hypothesis
    // stays unconfirmed without a rerun; the separately classified aggregate
    // records the understood infrastructure cause without claiming a pass.
    let environmental_attempts: Vec<&NodeAttempt> = retried
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.environmental")
        .collect();
    let first_failed = environmental_attempts
        .iter()
        .any(|a| a.attempt == 1 && a.ok == Some(false) && a.reported);
    let names_environment = environmental_attempts.iter().any(|attempt| {
        attempt.attempt == 1
            && attempt.retry_class.is_none()
            && attempt.environmental_class.as_deref() == Some("bpfjailer-banner")
    });
    if environmental_attempts.len() != 1
        || !first_failed
        || !names_environment
        || environmental_assessment(&retried.attempts, environmental_attempts[0])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
    {
        return Err(format!(
            "scheduler accounting: a one-shot environmental failure was retried or lost its unconfirmed classification: attempts={:?}",
            environmental_attempts
                .iter()
                .map(|a| (
                    a.attempt,
                    a.ok,
                    a.reported,
                    a.retry_class,
                    a.environmental_class.as_deref(),
                ))
                .collect::<Vec<_>>()
        ));
    }
    let environmental_outcome = retried
        .outcomes
        .iter()
        .find(|outcome| outcome.tag == "fixture.environmental")
        .ok_or("scheduler accounting: recovered environmental outcome disappeared")?;
    let environmental_gate =
        ledger_gate_with_attempts(environmental_outcome, &retried.attempts);
    if environmental_gate["result"] != "no_result"
        || environmental_gate["failure_class"] != "understood_infrastructure_failure"
        || environmental_gate["raw_result"] != "fail"
        || environmental_gate["raw_failure_class"] != "understood_infrastructure_failure"
        || environmental_gate["exit_code"] != 1
        || !environmental_gate["failure_origin"].is_null()
        || environmental_gate.get("failed_substeps").is_some()
        || environmental_gate["retries"] != 0
        || environmental_gate["attempts"].as_array().map(Vec::len) != Some(1)
        || environmental_gate["attempts"][0]["result"] != "fail"
        || environmental_gate["attempts"][0]["exit_code"] != 1
        || environmental_gate["attempts"][0]["reported"] != true
        || environmental_gate["attempts"][0]["execution"] != "completed"
        || environmental_gate["attempts"][0]["environmental_verdict"] != "unconfirmed"
        || !environmental_gate["attempts"][0]["retry_class"].is_null()
    {
        return Err(format!(
            "scheduler accounting: the ledger erased or misreported a one-shot environmental failure: {environmental_gate}"
        ));
    }
    // ⚠️ THE FLAKY WARNING MUST APPEAR ON A RUN THAT PASSED, and this is the
    // bracket that holds it there. `retried` above is GREEN — ok=true, no
    // failures — and it contains a node that failed once and recovered. That is
    // precisely the run on which a flake warning looks like noise and gets
    // dropped, and precisely the run where it is the only warning anyone gets
    // before the test fails for real.
    //
    // It also pins the two halves apart: the FLAKY block present, and the
    // FAILURE banner ABSENT. A summary that printed both would be telling the
    // reader a green run had failed.
    {
        let mut nextest_attempts = retried.attempts.clone();
        let mut retry_pass = environmental_attempts[0].clone();
        retry_pass.attempt = 2;
        retry_pass.ok = Some(true);
        retry_pass.returncode = Some(0);
        retry_pass.reason.clear();
        retry_pass.failure_class = None;
        retry_pass.failure_detail = None;
        retry_pass.environmental_class = None;
        retry_pass.detail_observed = true;
        nextest_attempts.push(retry_pass);
        for attempt in &mut nextest_attempts {
            if attempt.tag != "fixture.environmental" {
                continue;
            }
            if attempt.attempt == 1 {
                attempt.retry_class = Some(RetryClass::BpfjailerBanner);
            }
            attempt.test_results = Some(if attempt.attempt == 1 {
                vec![
                    dagrun::TestResult::new("hermit::fixture$hard_failure".into(), false, 1)
                        .map_err(|error| format!("end-of-run summary: {error}"))?,
                    dagrun::TestResult::new(
                        "hermit::fixture$recovered_on_retry".into(), false, 1,
                    )
                    .map_err(|error| format!("end-of-run summary: {error}"))?,
                ]
            } else {
                vec![
                    dagrun::TestResult::new("hermit::fixture$hard_failure".into(), false, 1)
                        .map_err(|error| format!("end-of-run summary: {error}"))?,
                    dagrun::TestResult::new(
                        "hermit::fixture$recovered_on_retry".into(), true, 1,
                    )
                    .map_err(|error| format!("end-of-run summary: {error}"))?,
                ]
            });
        }
        let nextest_nodes = BTreeSet::from(["fixture.environmental".to_string()]);
        let (observations, typed_errors) =
            nextest_test_observations(&nextest_attempts, &nextest_nodes);
        if !typed_errors.is_empty() || observations.len() != 4 {
            return Err(format!(
                "end-of-run summary: typed nextest results were not retained exactly: observations={observations:?}, errors={typed_errors:?}"
            ));
        }
        let mut missing_results = nextest_attempts.clone();
        missing_results[0].test_results = None;
        let (_, missing_errors) = nextest_test_observations(&missing_results, &nextest_nodes);
        if missing_errors.len() != 1
            || !missing_errors[0].contains("individual nextest results are UNKNOWN")
            || !missing_errors[0].contains("fixture.environmental attempt 1")
        {
            return Err(format!(
                "end-of-run summary: missing typed nextest results did not fail by node and attempt: {missing_errors:?}"
            ));
        }
        let mut unknown = RunSummary::new(Verdict::Pass, 0, "self-test", missing_errors);
        unknown.wall_s = Some(0.0);
        unknown.nodes_executed = 1;
        let unknown_rendered = run_summary_lines(&unknown, std::time::Instant::now()).join("\n");
        if !unknown_rendered.contains("retries and individual test results: UNKNOWN")
            || unknown_rendered.contains("no retries, no flaky tests")
        {
            return Err(format!(
                "end-of-run summary: missing typed results became a clean zero: {unknown_rendered}"
            ));
        }

        let mut inner_retry = nextest_attempts
            .iter()
            .find(|attempt| attempt.tag == "fixture.environmental" && attempt.attempt == 2)
            .cloned()
            .ok_or("end-of-run summary: no completed nextest attempt for retry fixture")?;
        inner_retry.tag = "fixture.nextest_inner".into();
        inner_retry.retry_class = None;
        inner_retry.test_results = Some(vec![
            dagrun::TestResult::new("hermit::fixture$inner_retry".into(), true, 2)
                .map_err(|error| format!("end-of-run summary: {error}"))?,
        ]);
        let inner_nodes = BTreeSet::from([inner_retry.tag.clone()]);
        let (inner_observations, inner_errors) =
            nextest_test_observations(std::slice::from_ref(&inner_retry), &inner_nodes);
        let inner_summary = test_id_summary(inner_observations, &[], &BTreeSet::new());
        if !inner_errors.is_empty()
            || inner_summary.recovered.len() != 1
            || inner_summary.recovered[0].inner_retry_occurrences != 1
            || inner_summary.retry_occurrences != 1
        {
            return Err(format!(
                "end-of-run summary: a nextest pass after an inner retry was not retained as one recovered retry: errors={inner_errors:?}, summary={inner_summary:?}"
            ));
        }
        let dbt_log = "[test.dbt_parity] ▶ START DynamoRIO DBT strict backend parity matrix\n\
[test.dbt_parity] PASS dbt/file_metadata: matched\n\
[test.dbt_parity] FAIL dbt/random_sources: output differed\n\
[test.dbt_parity] PASS dbt/: empty case\n\
[test.dbt_parity] PASS ptrace/wrong_backend: matched\n\
[test.dbt_parity] PASS dbt/missing_colon\n\
[test.dbt_parity] XPASS dbt/known_gap: candidate\n\
[test.other] FAIL dbt/other_node: ignored\n";
        let dbt = dbt_parity_test_observations(dbt_log);
        let expected_dbt = vec![
            TestAttemptObservation {
                node: DBT_PARITY_NODE.into(),
                attempt: 1,
                id: "backend-parity/file_metadata [dbt/strict]".into(),
                passed: true,
                inner_attempts: 1,
            },
            TestAttemptObservation {
                node: DBT_PARITY_NODE.into(),
                attempt: 1,
                id: "backend-parity/random_sources [dbt/strict]".into(),
                passed: false,
                inner_attempts: 1,
            },
        ];
        if dbt != expected_dbt {
            return Err(format!(
                "end-of-run summary: DBT parity PASS/FAIL rows or malformed-line refusal were \
                 parsed incorrectly: {dbt:?}"
            ));
        }
        let dbt_failed = test_id_summary(
            dbt,
            &[],
            &BTreeSet::from([DBT_PARITY_NODE.to_string()]),
        );
        if dbt_failed.failed.len() != 1
            || dbt_failed.failed[0].id != "backend-parity/random_sources [dbt/strict]"
            || !dbt_failed.failed_nodes_without_test_ids.is_empty()
        {
            return Err(format!(
                "end-of-run summary: a DBT parity case failure did not replace its node-only \
                 fallback with the stable test id: {dbt_failed:?}"
            ));
        }

        let dbt_retry_log = "[test.dbt_parity] ▶ START first attempt\n\
[test.dbt_parity] FAIL dbt/virtual_clock: first attempt failed\n\
[test.dbt_parity] ▶ START second attempt\n\
[test.dbt_parity] PASS dbt/virtual_clock: retry passed\n";
        let dbt_retry = dbt_parity_test_observations(dbt_retry_log);
        if dbt_retry.len() != 2
            || dbt_retry[0].attempt != 1
            || dbt_retry[0].passed
            || dbt_retry[1].attempt != 2
            || !dbt_retry[1].passed
            || dbt_retry[0].id != dbt_retry[1].id
        {
            return Err(format!(
                "end-of-run summary: DBT parity retry lost its per-attempt result: {dbt_retry:?}"
            ));
        }
        let mut dbt_attempts = vec![
            unreported_attempt(DBT_PARITY_NODE.into(), 1),
            unreported_attempt(DBT_PARITY_NODE.into(), 2),
        ];
        dbt_attempts[0].retry_class = Some(RetryClass::AlwaysEligible);
        dbt_attempts[0].retry_detail = Some("self-test retry".into());
        let dbt_retry_summary = test_id_summary(dbt_retry, &dbt_attempts, &BTreeSet::new());
        if dbt_retry_summary.recovered.len() != 1
            || dbt_retry_summary.recovered[0].id
                != "backend-parity/virtual_clock [dbt/strict]"
            || dbt_retry_summary.recovered[0].retry_classes
                != [RetryClass::AlwaysEligible]
        {
            return Err(format!(
                "end-of-run summary: DBT parity fail-then-pass was not retained as recovered: \
                 {dbt_retry_summary:?}"
            ));
        }

        let dbt_pre_case_death = dbt_parity_test_observations(
            "[test.dbt_parity] ▶ START DynamoRIO DBT strict backend parity matrix\n\
[test.dbt_parity] ERROR: process died before the first case result\n",
        );
        let dbt_pre_case_summary = test_id_summary(
            dbt_pre_case_death,
            &[],
            &BTreeSet::from([DBT_PARITY_NODE.to_string()]),
        );
        if dbt_pre_case_summary.failed_nodes_without_test_ids != [DBT_PARITY_NODE] {
            return Err(format!(
                "end-of-run summary: a DBT parity node that died before its first case gained an \
                 invented test id: {dbt_pre_case_summary:?}"
            ));
        }
        let e2e_root = tmp.join("summary-e2e");
        std::fs::create_dir_all(&e2e_root)
            .map_err(|e| format!("end-of-run summary: cannot create E2E fixture: {e}"))?;
        std::fs::write(
            e2e_root.join("results.jsonl"),
            "{\"attempt\":1,\"test\":\"applications/example\",\"category\":\"applications\",\"lane\":\"portable\",\"mode\":\"verify\",\"backend\":\"ptrace\",\"outcome\":\"FAIL\"}\n\
{\"attempt\":2,\"test\":\"applications/example\",\"category\":\"applications\",\"lane\":\"portable\",\"mode\":\"verify\",\"backend\":\"ptrace\",\"outcome\":\"PASS\"}\n",
        )
        .map_err(|e| format!("end-of-run summary: cannot write E2E fixture: {e}"))?;
        let e2e = e2e_test_observations(&e2e_root)?;
        if e2e.len() != 2
            || e2e[0].node != "e2e.manifest_applications"
            || e2e[0].id != "applications/example [ptrace/verify]"
            || e2e[0].passed
            || !e2e[1].passed
        {
            return Err(format!(
                "end-of-run summary: E2E rows did not retain test id, backend, mode, attempt, \
                node, and verdict: {e2e:?}"
            ));
        }
        let e2e_retry_summary = test_id_summary(e2e, &[], &BTreeSet::new());
        if e2e_retry_summary.recovered.len() != 1
            || e2e_retry_summary.recovered[0].id != "applications/example [ptrace/verify]"
            || e2e_retry_summary.recovered[0].retry_classes
                != [RetryClass::AlwaysEligible]
            || e2e_retry_summary.retry_occurrences != 1
        {
            return Err(format!(
                "end-of-run summary: inner E2E fail-then-pass was not reported as one recovered retry: {e2e_retry_summary:?}"
            ));
        }
        let mut e2e_green = RunSummary::new(Verdict::Pass, 0, "self-test", Vec::new());
        e2e_green.flaky = e2e_retry_summary.recovered.clone();
        e2e_green.retry_occurrences = e2e_retry_summary.retry_occurrences;
        e2e_green.individual_test_results_complete = true;
        e2e_green.wall_s = Some(0.0);
        e2e_green.nodes_executed = 1;
        let e2e_rendered = run_summary_lines(&e2e_green, std::time::Instant::now()).join("\n");
        if !e2e_rendered.contains(
            "applications/example [ptrace/verify] (node e2e.manifest_applications)  (1 retry",
        )
            || !e2e_rendered
                .contains("retries: 1 occurrence(s) recorded from scheduler and per-cell attempts")
        {
            return Err(format!(
                "end-of-run summary: inner-only retry was not rendered with truthful provenance: \
                 {e2e_rendered}"
            ));
        }
        let failed_nodes = BTreeSet::from(["fixture.environmental".to_string()]);
        let split = test_id_summary(observations, &nextest_attempts, &failed_nodes);
        if split.recovered.len() != 1
            || split.recovered[0].id != "hermit::fixture$recovered_on_retry"
            || split.recovered[0].retry_classes != [RetryClass::BpfjailerBanner]
            || split.failed.len() != 1
            || split.failed[0].id != "hermit::fixture$hard_failure"
            || split.failed[0].retry_classes != [RetryClass::BpfjailerBanner]
            || !split.failed_nodes_without_test_ids.is_empty()
            || split.retry_occurrences != 1
        {
            return Err(format!(
                "end-of-run summary: failed and recovered test ids were conflated, retry counts \
                 did not come from attempts[].retry_class, or a node tag leaked into the test-id \
                 list: {split:?}"
            ));
        }

        // One test id can be emitted by more than one DAG node. The node is
        // part of the producer's identity, so a passing peer must never erase a
        // failing node merely because its tag sorts later. Exercise both lexical
        // orders: the old id-only grouping failed one of these two cases.
        for (failing_node, passing_node) in [("a.fail", "z.pass"), ("z.fail", "a.pass")] {
            let shared_id = "shared::binary$same_test";
            let peer_summary = test_id_summary(
                vec![
                    TestAttemptObservation {
                        node: failing_node.into(),
                        attempt: 1,
                        id: shared_id.into(),
                        passed: false,
                        inner_attempts: 1,
                    },
                    TestAttemptObservation {
                        node: passing_node.into(),
                        attempt: 1,
                        id: shared_id.into(),
                        passed: true,
                        inner_attempts: 1,
                    },
                ],
                &[],
                &BTreeSet::from([failing_node.to_string()]),
            );
            if peer_summary.failed.len() != 1
                || peer_summary.failed[0].node != failing_node
                || peer_summary.failed[0].id != shared_id
                || !peer_summary.recovered.is_empty()
                || !peer_summary.failed_nodes_without_test_ids.is_empty()
            {
                return Err(format!(
                    "end-of-run summary: test id {shared_id} from peer node {passing_node} changed \
                     the terminal result for failing node {failing_node}: {peer_summary:?}"
                ));
            }
        }
        let mut red = RunSummary::new(Verdict::Fail, 1, "self-test", Vec::new());
        red.flaky = split.recovered.clone();
        red.failed_ids = split.failed.clone();
        red.retry_occurrences = split.retry_occurrences;
        red.individual_test_results_complete = true;
        red.wall_s = Some(0.0);
        red.nodes_executed = 1;
        let red_rendered = run_summary_lines(&red, std::time::Instant::now()).join("\n");
        if !red_rendered.contains(SUMMARY_FLAKY_HEADING)
            || !red_rendered.contains("1 test id(s) recovered")
            || !red_rendered.contains("1 test id(s) failed and did NOT recover")
            || !red_rendered.contains(
                "hermit::fixture$recovered_on_retry (node fixture.environmental)  (1 retry",
            )
            || !red_rendered.contains(
                "hermit::fixture$hard_failure (node fixture.environmental)  (1 retry",
            )
            || !red_rendered.contains(
                "retries: 1 occurrence(s) recorded from scheduler and per-cell attempts",
            )
        {
            return Err(format!(
                "end-of-run summary: one hard failure and one recovered test id were not shown \
                 as separate counts with their retry counts: {red_rendered}"
            ));
        }

        let mut green = RunSummary::new(Verdict::Pass, 0, "self-test", Vec::new());
        green.flaky = split.recovered.clone();
        green.retry_occurrences = split.retry_occurrences;
        green.individual_test_results_complete = true;
        green.wall_s = Some(0.0);
        green.nodes_executed = 1;
        let rendered = run_summary_lines(&green, std::time::Instant::now()).join("\n");
        let names_the_test = rendered.contains("hermit::fixture$recovered_on_retry");
        let names_the_ground = rendered.contains("bpfjailer-banner");
        if !rendered.contains(SUMMARY_FLAKY_HEADING)
            || !names_the_test
            || !names_the_ground
            || rendered.contains("❌ FAILURE")
        {
            return Err(format!(
                "end-of-run summary: a GREEN run carrying a recovered flake did not warn about \
                 it, or warned as a failure: flaky={:?} heading={} test={} \
                 ground={} failure_banner={}",
                split.recovered,
                rendered.contains(SUMMARY_FLAKY_HEADING),
                names_the_test,
                names_the_ground,
                rendered.contains("❌ FAILURE"),
            ));
        }
        // ⚠️ THE THIRD STATE, WHICH THE TWO CHECKS AROUND IT DO NOT COVER: a run
        // with nothing to report must SAY nothing was found, not render nothing.
        // Both blocks above are conditional with no else, so before this a clean
        // run and a run whose retry accounting produced nothing were the same
        // bytes -- absence readable as a result, on the one section the owner
        // reads specifically to spot a flaky pass.
        let mut clean = RunSummary::new(Verdict::Pass, 0, "self-test", Vec::new());
        clean.wall_s = Some(0.0);
        clean.nodes_executed = 3;
        clean.individual_test_results_complete = true;
        let clean_rendered = run_summary_lines(&clean, std::time::Instant::now()).join("\n");
        if !clean_rendered.contains("no retries, no flaky tests, and no failed test ids")
            || clean_rendered.contains(SUMMARY_FLAKY_HEADING)
            || clean_rendered.contains("\u{274c} FAILURE")
        {
            return Err(format!(
                "end-of-run summary: a CLEAN run must state that it found no retries and no \
                 flaky tests, so silence cannot be confused with the accounting having \
                 produced nothing: stated={} flaky_heading={} failure_banner={}",
                clean_rendered.contains("no retries, no flaky tests, and no failed test ids"),
                clean_rendered.contains(SUMMARY_FLAKY_HEADING),
                clean_rendered.contains("\u{274c} FAILURE"),
            ));
        }
        // And it must NOT claim cleanliness before a DAG ran, which would be the
        // same error inverted: nothing was counted, so nothing can be reported.
        let mut nothing_ran = RunSummary::new(Verdict::Refused, 2, "self-test", Vec::new());
        nothing_ran.wall_s = None;
        let nothing_rendered =
            run_summary_lines(&nothing_ran, std::time::Instant::now()).join("\n");
        if nothing_rendered.contains("no retries, no flaky tests, and no failed test ids") {
            return Err(
                "end-of-run summary: a run that stopped before the DAG claimed it found no \
                 flaky tests; it counted nothing and must claim nothing"
                    .to_string(),
            );
        }

    }
    // Manifest cells own their retries inside test-harness. Once a manifest
    // node has executed, the outer scheduler must not run that node again: an
    // outer retry would rerun passing peers and restart the inner attempt
    // ordinals at one.
    let e2e_attempts = tmp.join("e2e-attempts");
    let e2e_log = tmp.join("e2e-attempts.log");
    let structured_counts = serde_json::json!({
        "schema": 2,
        "executed_tests": 2,
        "filtered_tests": 0,
        "results": [
            {"id": "failing-cell", "result": "fail", "attempts": 2},
            {"id": "passing-peer", "result": "pass", "attempts": 1},
        ],
    })
    .to_string();
    let e2e_cmd = format!(
        "printf '%s\\n' {} > \"$DAGRUN_TEST_COUNTS_PATH\"; printf '%s\\n' run >> {}; exit 1",
        validate_plan::shell_quote(&structured_counts),
        validate_plan::shell_quote(&e2e_attempts.to_string_lossy()),
    );
    let mut e2e_step = step("manifest_attempt", &e2e_cmd);
    e2e_step.group = "e2e".into();
    e2e_step.job = "manifest_attempt".into();
    let e2e_manifest = DagManifest {
        lane: "portable".into(),
        category: "applications".into(),
        test: None,
        mode: None,
        backend: None,
    };
    e2e_step.manifest = Some(e2e_manifest.clone());
    e2e_step.result_manifests = Some(vec![
        dagrun::model::ResultManifest::ManifestCell(e2e_manifest),
        dagrun::model::ResultManifest::StructuredTestResults(
            dagrun::model::StructuredTestResultsManifest::current("e2e.manifest_attempt"),
        ),
    ]);
    let e2e_retry = run_lane_once(
        &DagConfig { steps: vec![e2e_step], ..Default::default() },
        1,
        true,
        0,
        None,
        &e2e_log,
        None,
        false,
    );
    let recorded_attempts = std::fs::read_to_string(&e2e_attempts)
        .map_err(|e| format!("scheduler accounting: cannot read E2E attempt fixture: {e}"))?;
    let e2e_node_attempts = e2e_retry
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "e2e.manifest_attempt")
        .collect::<Vec<_>>();
    let e2e_results = e2e_node_attempts
        .first()
        .and_then(|attempt| attempt.test_results.as_ref());
    if e2e_retry.ok
        || e2e_node_attempts.len() != 1
        || e2e_results
            != Some(&vec![
                dagrun::TestResult::new("failing-cell".into(), false, 2)?,
                dagrun::TestResult::new("passing-peer".into(), true, 1)?,
            ])
        || recorded_attempts.lines().collect::<Vec<_>>() != ["run"]
    {
        return Err(format!(
            "scheduler accounting: manifest framework retry did not preserve the 2/1/1 proof: \
             ok={} node_attempts={} results={e2e_results:?} rows={recorded_attempts:?}",
            e2e_retry.ok, e2e_node_attempts.len()
        ));
    }

    // The same real scheduler node without typed manifest identity also gets
    // one outer execution. Test frameworks may retry the failing test inside
    // that execution; validate never repeats the enclosing node or its peers.
    let ordinary_attempts = tmp.join("ordinary-structured-attempts");
    let ordinary_log = tmp.join("ordinary-structured-attempts.log");
    let ordinary_cmd = format!(
        "printf '%s\\n' {} > \"$DAGRUN_TEST_COUNTS_PATH\"; printf '%s\\n' run >> {}; exit 1",
        validate_plan::shell_quote(&structured_counts),
        validate_plan::shell_quote(&ordinary_attempts.to_string_lossy()),
    );
    let mut ordinary_step = step("ordinary_structured", &ordinary_cmd);
    ordinary_step.result_manifests = Some(vec![
        dagrun::model::ResultManifest::StructuredTestResults(
            dagrun::model::StructuredTestResultsManifest::current("fixture.ordinary_structured"),
        ),
    ]);
    let ordinary_retry = run_lane_once(
        &DagConfig { steps: vec![ordinary_step], ..Default::default() },
        1,
        true,
        0,
        None,
        &ordinary_log,
        None,
        false,
    );
    let ordinary_node_attempts = ordinary_retry
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.ordinary_structured")
        .collect::<Vec<_>>();
    let ordinary_runs = std::fs::read_to_string(&ordinary_attempts)
        .map_err(|e| format!("scheduler accounting: cannot read ordinary retry fixture: {e}"))?;
    if ordinary_retry.ok
        || ordinary_node_attempts.len() != 1
        || ordinary_node_attempts.iter().any(|attempt| {
            attempt.test_results.as_ref().is_none_or(|results| {
                results.iter().find(|result| result.id == "passing-peer").is_none_or(|peer| {
                    !peer.passed || peer.attempts != 1
                })
            })
        })
        || ordinary_runs.lines().count() != 1
    {
        return Err(format!(
            "scheduler accounting: ordinary-node control received an outer retry or lost its structured results: \
             ok={} node_attempts={} rows={ordinary_runs:?}",
            ordinary_retry.ok,
            ordinary_node_attempts.len()
        ));
    }

    Ok(())
    })();

    match prior_dagrun_log_dir {
        Some(value) => unsafe { std::env::set_var("DAGRUN_LOG_DIR", value) },
        None => unsafe { std::env::remove_var("DAGRUN_LOG_DIR") },
    }

    let cleanup = std::fs::remove_dir_all(&tmp)
        .map_err(|e| format!("scheduler accounting: cannot remove {}: {e}", tmp.display()));
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(
            "scheduler accounting: complete and allowed-failure plans accepted; fail-fast, \
             skipped, aborted, one-shot outer execution, inner manifest retry identity, and \
             terminal failure/incompleteness summary bracketed"
                .into(),
        ),
        (Err(problem), Ok(())) => Err(problem),
        (Ok(()), Err(cleanup_problem)) => Err(cleanup_problem),
        (Err(problem), Err(cleanup_problem)) => Err(format!(
            "{problem}; cleanup also failed: {cleanup_problem}"
        )),
    }
}

/// Run one lane exactly once and retain every reported or missing outcome.
///
/// Test frameworks own retries at the individual-test or manifest-cell layer.
/// Retrying an outer DAG node repeats passing peers and cannot repair the first
/// graph's dependency skips: RUN 1572 retried `gate.manifest` successfully while
/// its sixteen dependents remained unexecuted. A failed or incomplete graph
/// therefore stays failed or incomplete, with its original evidence preserved.
fn run_lane_once(
    cfg: &DagConfig,
    jobs: i64,
    keep_going: bool,
    verbosity: i64,
    cgroups: BoxedCgroups,
    log_path: &Path,
    deadline: Option<u64>,
    record_step_profiles: bool,
) -> LaneResult {
    let expired_before_dispatch = || {
        eprintln!(
            "validate: whole-run budget expired during setup; no DAG node will be started \
             unbounded, and every planned node is recorded as not attempted"
        );
        LaneResult {
            outcomes: Vec::new(),
            skipped: cfg.steps.iter().map(|step| step.tag()).collect(),
            attempts: Vec::new(),
            complete: false,
            ok: false,
            run_timed_out: true,
        }
    };
    if remaining_budget_s(deadline) == Some(0) {
        return expired_before_dispatch();
    }

    let log_start = settled_log_len(log_path);
    let cpu_budget = scheduler_cpu_budget();
    // Settling the output can consume the remaining shared allowance. The
    // scheduler API treats zero as unbounded, so recheck after all setup and
    // immediately before dispatch. Flooring a positive sub-second remainder
    // must refuse launch rather than create a fresh second or remove the bound.
    let remaining = remaining_budget_s(deadline);
    if remaining == Some(0) {
        return expired_before_dispatch();
    }
    let result = run_dag_boxed_deadline(
        cfg,
        jobs,
        keep_going,
        verbosity,
        cgroups,
        None,
        Some(cpu_budget),
        remaining,
    );
    if record_step_profiles {
        forward_step_profiles(&result, jobs);
    }

    let run_timed_out = result.run_timed_out;
    let mut scheduler_not_launched = BTreeSet::new();
    let mut refused = BTreeSet::new();
    let planned: Vec<String> = cfg.steps.iter().map(|step| step.tag()).collect();
    update_not_run_explanations(
        &planned,
        result.outcomes.len(),
        result.skipped.len(),
        &result.not_launched,
        result.intentional_skips.len(),
        &mut scheduler_not_launched,
        &mut refused,
    );

    let outcomes = result.outcomes;
    let skipped = result.skipped;
    let by_tag: BTreeMap<String, StepOutcome> = outcomes
        .iter()
        .map(|outcome| (outcome.tag.clone(), outcome.clone()))
        .collect();
    let mut attempts: Vec<NodeAttempt> = outcomes
        .iter()
        .map(|outcome| reported_attempt(outcome, 1))
        .collect();
    let unreported = unreported_non_intentional_steps(cfg, &by_tag, &skipped);
    attempts.extend(
        unreported
            .iter()
            .cloned()
            .map(|tag| unreported_attempt(tag, 1)),
    );

    // Keep environmental and producer-owned failure classification as evidence,
    // but never use it to grant a second outer execution. Without a rerun an
    // environmental hypothesis remains UNCONFIRMED. Only the stricter terminal
    // diagnostic matcher can establish infrastructure failure; typed failed tests
    // and completed node budget breaches still take precedence over that diagnosis.
    if let Some(log) = read_log_since_settled(log_path, log_start) {
        for outcome in outcomes
            .iter()
            .filter(|outcome| outcome_is_failure(outcome))
        {
            if let Some(detail) = validate_runtime::extract_node_detail(&log, &outcome.tag) {
                let class = validate_runtime::environmental_block_class(&detail);
                let failure = validate_runtime::failure_class_from_detail(&detail);
                stamp_attempt_detail(
                    &mut attempts, &outcome.tag, class,
                    validate_runtime::understood_infrastructure_class(&detail), failure,
                );
                if class.is_some() {
                    if let Some(line) =
                        terminal_environmental_observation(&attempts, &outcome.tag)
                    {
                        println!("{line}");
                    }
                }
            }
        }
    }

    let (refused_before_launching, remaining) = partition_unreported(&unreported, &refused);
    let (not_launched, unaccounted) =
        partition_unreported(&remaining, &scheduler_not_launched);
    if !not_launched.is_empty() {
        eprintln!("{}", scheduler_not_launched_message(&not_launched));
    }
    if !refused_before_launching.is_empty() {
        eprintln!(
            "validate: {} planned node(s) DID NOT RUN because the scheduler REFUSED before \
             launching anything; the refusal above states the reason. The lane is incomplete \
             and cannot be green: {}.",
            refused_before_launching.len(),
            refused_before_launching.join(", ")
        );
    }
    if !unaccounted.is_empty() {
        eprintln!(
            "validate: ERROR: scheduler returned without an outcome, a dependency-skip, or a \
             scheduler not-launched result for {} non-intentional planned node(s): {}. These are \
             UNACCOUNTED FOR -- nothing explains why they did not run. The lane is incomplete \
             and cannot be green.",
            unaccounted.len(),
            unaccounted.join(", ")
        );
    }

    let complete = !run_timed_out
        && unreported.is_empty()
        && skipped.is_empty()
        && outcomes
            .iter()
            .all(|outcome| outcome_execution(outcome) == AttemptExecution::Completed);
    let ok = outcomes.iter().all(|outcome| outcome.ok || outcome.aborted);
    LaneResult {
        outcomes,
        skipped,
        attempts,
        complete,
        ok,
        run_timed_out,
    }
}

/// Print every node that took more than one attempt, and every attempt that
/// reported nothing.
///
/// This is the human-facing half of the same fact the ledger now carries. It is
/// printed even when the run is GREEN, which is the whole point: a green that
/// needed a second attempt is the case that used to leave no trace anywhere
/// except a lane-level counter that names no node.
fn retry_attempt_line(
    attempts: &[NodeAttempt],
    row: &NodeAttempt,
    total_attempts: usize,
) -> String {
    let verdict = attempt_result(row).unwrap_or("unknown step result");
    let because = row
        .retry_class
        .map(|class| {
            let detail = row
                .retry_detail
                .as_deref()
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            format!(" — retried because: {}{detail}", class.as_str())
        })
        .unwrap_or_default();
    let detail = if row.reason.is_empty() {
        String::new()
    } else {
        format!(" [{}]", row.reason.trim())
    };
    let environmental = match environmental_assessment(attempts, row) {
        Some((validate_runtime::EnvBlockVerdict::Confirmed, _)) => format!(
            " — ENVIRONMENTAL CONFIRMED ({}): an actual re-execution passed",
            row.environmental_class.as_deref().unwrap_or("unknown")
        ),
        Some((validate_runtime::EnvBlockVerdict::Refuted, Some(shape))) => {
            let attribution = match shape {
                validate_runtime::RefutedShape::BannerGone => {
                    "the environmental banner was gone and the node still failed"
                }
                validate_runtime::RefutedShape::Persistent => {
                    "the same environmental signature persisted"
                }
                validate_runtime::RefutedShape::SignatureChanged => {
                    "the environmental signature changed"
                }
            };
            format!(
                " — ENVIRONMENTAL REFUTED ({}; {}): {attribution}",
                row.environmental_class.as_deref().unwrap_or("unknown"),
                shape.as_str()
            )
        }
        Some((validate_runtime::EnvBlockVerdict::Unconfirmed, _)) => format!(
            " — ENVIRONMENTAL UNCONFIRMED ({}): no actual re-execution completed; this remains an \
             unsettled RED",
            row.environmental_class.as_deref().unwrap_or("unknown")
        ),
        Some((validate_runtime::EnvBlockVerdict::Refuted, None)) => format!(
            " — ENVIRONMENTAL REFUTED ({}; shape unknown): an actual re-execution failed, but \
             emitted no attempt-local detail region for banner attribution",
            row.environmental_class.as_deref().unwrap_or("unknown")
        ),
        None => String::new(),
    };
    format!(
        "  {} attempt {}/{}: {verdict} ({:.1}s){detail}{because}{environmental}",
        row.tag, row.attempt, total_attempts, row.duration_s
    )
}

fn terminal_environmental_observation(attempts: &[NodeAttempt], tag: &str) -> Option<String> {
    let attempt = attempts
        .iter()
        .rev()
        .find(|attempt| attempt.tag == tag && attempt.environmental_class.is_some())?;
    let class = attempt.environmental_class.as_deref()?;
    let verdict = environmental_assessment(attempts, attempt)?.0;
    let verdict = match verdict {
        validate_runtime::EnvBlockVerdict::Confirmed => "CONFIRMED",
        validate_runtime::EnvBlockVerdict::Refuted => "REFUTED",
        validate_runtime::EnvBlockVerdict::Unconfirmed => "UNCONFIRMED",
    };
    Some(format!(
        "🧱 {tag}: observed environmental signature {class} on attempt {}; terminal hypothesis \
         {verdict}; aggregate result is reported separately in the node table.",
        attempt.attempt
    ))
}

fn print_retry_ledger(attempts: &[NodeAttempt]) {
    let mut retried: BTreeMap<&str, Vec<&NodeAttempt>> = BTreeMap::new();
    for attempt in attempts {
        retried.entry(attempt.tag.as_str()).or_default().push(attempt);
    }
    retried.retain(|_, rows| {
        rows.len() > 1
            || rows
                .iter()
                .any(|row| !row.reported || row.environmental_class.is_some())
    });
    if retried.is_empty() {
        return;
    }
    println!(
        "\nRetry and environmental verdict ledger ({} node(s)): every attempt is listed, \
         including the ones a later attempt superseded.",
        retried.len()
    );
    for rows in retried.values() {
        for row in rows {
            println!("{}", retry_attempt_line(attempts, row, rows.len()));
        }
    }
}

/// Was this node killed by its wall or CPU budget?
///
/// The scheduler retains these facts independently of its human-readable
/// reason. Presentation text may contain the word "timeout" while explicitly
/// saying no timeout occurred, and an OOM message may hide a simultaneous
/// budget breach because the display has a precedence order.
fn outcome_hit_its_budget(outcome: &StepOutcome) -> bool {
    outcome.timed_out || outcome.cpu_timed_out
}

/// Pin budget detection to the typed producer facts, not its text.
///
/// The reason is deliberately replaced after construction. If this test ever
/// starts following presentation text again, the positive and negative arms
/// fail independently.
fn budget_reason_bracket() -> Result<String, String> {
    use dagrun::model::step_failure_reason;
    // (label, returncode, oomed, timed_out, pids_tripped, detail_failure, cpu_timed_out)
    let cases: &[(&str, Option<i64>, bool, bool, bool, bool, bool)] = &[
        ("wall budget", None, false, true, false, false, false),
        ("cpu budget", None, false, false, false, false, true),
        ("oom kill", Some(-9), true, false, false, false, false),
        ("pids guard", None, false, false, true, false, false),
        ("detail capture", None, false, false, false, true, false),
        ("SIGSEGV", Some(-11), false, false, false, false, false),
        ("SIGABRT", Some(-6), false, false, false, false, false),
        ("SIGKILL", Some(-9), false, false, false, false, false),
        ("ordinary exit", Some(1), false, false, false, false, false),
        ("no exit collected", None, false, false, false, false, false),
    ];
    let mut eligible = Vec::new();
    for (label, rc, oomed, timed_out, pids, detail, cpu_timed_out) in cases {
        let detail_rows: Vec<String> = if *detail {
            vec!["fixture detail write failed".to_string()]
        } else {
            Vec::new()
        };
        let reason = step_failure_reason(
            *rc,
            *oomed,
            if *oomed { 1 } else { 0 },
            *timed_out,
            600,
            *pids,
            pids.then_some("fixture pids guard"),
            &detail_rows,
            *cpu_timed_out,
            300,
            300,
            1.0,
            "",
        );
        let want = *timed_out || *cpu_timed_out;
        let mut outcome = StepOutcome::failed(
            (*label).into(),
            0.0,
            String::new(),
            *rc,
            *oomed,
            if *oomed { 1 } else { 0 },
            *timed_out,
            600,
            *cpu_timed_out,
            300,
            300,
            1.0,
            "",
            false,
            None,
            None,
        );
        outcome.reason = reason.clone();
        let got = outcome_hit_its_budget(&outcome);
        if got != want {
            return Err(format!(
                "budget reason: {label} rendered {reason:?}; retry-eligible={got} but the typed                  inputs say timed_out={timed_out} cpu_timed_out={cpu_timed_out}. A reason that                  merely MENTIONS a timeout is not a timeout."
            ));
        }
        if got {
            eligible.push(*label);
        }
        // Test both misleading positive words and missing classification words.
        // Every one of the ten original producer cases must retain its facts.
        for presentation in [
            "TIMEOUT >1s",
            "CPU-TIMEOUT >1s",
            "OOM-KILLED (hit inner MemoryMax; 1 oom_kill event(s))",
            "unrelated presentation",
            "",
        ] {
            outcome.reason = presentation.into();
            if outcome_hit_its_budget(&outcome) != want || outcome.oomed != *oomed {
                return Err(format!(
                    "budget reason: {label} followed presentation text {presentation:?}"
                ));
            }
        }
    }
    // The negative direction, stated as its own assertion rather than left
    // implicit in the loop: the signal arm is the one that used to misclassify,
    // and it must stay ineligible even though its text contains "timeout".
    let segv = step_failure_reason(
        Some(-11),
        false,
        0,
        false,
        600,
        false,
        None,
        &[],
        false,
        300,
        300,
        1.0,
        "",
    );
    if !segv.contains("timeout") {
        return Err(format!(
            "budget reason: the signal arm no longer contains the word 'timeout' ({segv:?}); this              bracket exists because it DOES, so re-check the producer before relaxing it"
        ));
    }
    let mut segv_outcome = StepOutcome::failed(
        "segv".into(),
        0.0,
        String::new(),
        Some(-11),
        false,
        0,
        false,
        600,
        false,
        300,
        300,
        1.0,
        "",
        false,
        None,
        None,
    );
    segv_outcome.reason = segv.clone();
    if outcome_hit_its_budget(&segv_outcome) {
        return Err(format!(
            "budget reason: signal-killed reason {segv:?} is retry-eligible"
        ));
    }
    // The producer's presentation has a precedence order; all independent
    // termination facts must survive that order and later detail replacement.
    for bits in 0_u8..8 {
        let oomed = bits & 1 != 0;
        let timed_out = bits & 2 != 0;
        let cpu_timed_out = bits & 4 != 0;
        let mut outcome = StepOutcome::failed(
            format!("combined-{bits}"),
            0.0,
            String::new(),
            Some(-9),
            oomed,
            if oomed { 2 } else { 0 },
            timed_out,
            600,
            cpu_timed_out,
            300,
            300,
            1.0,
            "",
            false,
            None,
            None,
        );
        let expected_prefix = if oomed {
            "OOM-KILLED"
        } else if cpu_timed_out {
            "CPU-TIMEOUT"
        } else if timed_out {
            "TIMEOUT"
        } else {
            "received SIGKILL"
        };
        if !outcome.reason.starts_with(expected_prefix) {
            return Err(format!(
                "budget reason: producer precedence changed for {bits}: {:?}",
                outcome.reason
            ));
        }
        let expected_class = if bits == 0 {
            FailureClass::ProductFailure
        } else {
            FailureClass::NoResult
        };
        for presentation in [
            outcome.reason.clone(),
            "unrelated presentation".into(),
            "TIMEOUT >1s OOM-KILLED".into(),
            String::new(),
        ] {
            outcome.reason = presentation;
            if outcome_hit_its_budget(&outcome) != (timed_out || cpu_timed_out)
                || outcome_failure_class(&outcome) != Some(expected_class)
            {
                return Err(format!(
                    "budget reason: independent facts or failure attribution lost for {bits}: {:?}",
                    outcome.reason
                ));
            }
        }
    }
    Ok(format!(
        "budget reason: 10 producer-rendered reason(s) classified; retry-eligible = {eligible:?};          the SIGSEGV arm contains the word \"timeout\" and is correctly NOT eligible; all eight independent OOM/CPU/wall combinations retain their typed attribution through poisoned presentation text"
    ))
}

/// Nodes the runner reported as killed by their wall or CPU budget.
fn timed_out_nodes(outcomes: &[StepOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| outcome_hit_its_budget(o))
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
    git_depth: u64,
    git_ahead: i64,
    git_behind: i64,
    commit_anchored: bool,
    /// Dirt present in the source at admission, plus final dirt for an in-place
    /// run. A clean admitted disposable checkout's later state is diagnostic.
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
    /// Retry ROUNDS executed for retry-eligible failures; `0` for a clean first pass.
    retry_rounds: u64,
    /// Whether THIS run observed the `pre.reverie_pin` gate pass. Recorded on the
    /// row itself so a reader never has to infer from a bare `pass` that the
    /// archival pin was proved current; the receipt verifier keys on it.
    reverie_pin_current: bool,
    /// Libtest counts aggregated from typed step outcomes; `None` is UNKNOWN.
    executed_tests: Option<i64>,
    /// Tests that passed, derived only from runner-owned typed outcomes.
    /// A failed retained count-only result cannot supply this value.
    passed_tests: Option<i64>,
    filtered_tests: Option<i64>,
}

struct ReceiptEvidence {
    base_sha: serde_json::Value,
    base_tree: serde_json::Value,
    reverie_base_sha: serde_json::Value,
    reverie_base_tree: serde_json::Value,
}

impl Default for ReceiptEvidence {
    fn default() -> Self {
        Self {
            base_sha: serde_json::Value::Null,
            base_tree: serde_json::Value::Null,
            reverie_base_sha: serde_json::Value::Null,
            reverie_base_tree: serde_json::Value::Null,
        }
    }
}

/// Ask the parent's single receipt finalizer for base identities.
/// Any missing helper, failed command, or malformed output stays explicit null;
/// the schema-5 consumer then refuses qualification.
fn receipt_evidence(
    tool_root: Option<&Path>,
    root: &Path,
    log: &Path,
    commit: &str,
) -> ReceiptEvidence {
    let Some(tool_root) = tool_root else { return ReceiptEvidence::default() };
    let helper = tool_root.join("ci-hub/validate/finalize_receipt.py");
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
    let field = |name: &str| value.get(name).cloned().unwrap_or(serde_json::Value::Null);
    ReceiptEvidence {
        base_sha: field("base_sha"),
        base_tree: field("base_tree"),
        reverie_base_sha: field("reverie_base_sha"),
        reverie_base_tree: field("reverie_base_tree"),
    }
}

/// Ask the parent lock authority whether this exact run is admitted. Ordinary
/// validation requires canonical current-main authority; frozen validation
/// requires a fully checked lock holder that remains explicitly noncanonical.
/// Production never trusts caller-supplied owner PIDs or sidecar paths. The
/// stop-test JSON seam is confined to an intrinsically non-qualifying fixture.
fn validate_lock_admission(
    tool_root: Option<&Path>,
    commit: &str,
    host: &str,
) -> Result<(), String> {
    let status = if env_flag("HERMIT_VALIDATE_STOP_TEST_MODE", "1") {
        let Ok(fixture) = std::env::var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON") else {
            return Err("stop-test mode is on but no planted authority status was supplied".into());
        };
        fixture.into_bytes()
    } else {
        let Some(tool_root) = tool_root else {
            return Err("no dev-hermit tool root was detected".into());
        };
        let ci_hub = tool_root.join("ci-hub/ci-hub");
        if !ci_hub.is_file() {
            return Err(format!(
                "the canonical launcher is missing at {}",
                ci_hub.display()
            ));
        }
        let Ok(output) = Command::new(&ci_hub)
            .args(["validate-lock", "authority-status", "--json"])
            .output()
        else {
            return Err(format!("could not execute {}", ci_hub.display()));
        };
        if !output.status.success() {
            return Err(format!(
                "`ci-hub validate-lock authority-status --json` exited {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "by signal".into(), |c| c.to_string())
            ));
        }
        output.stdout
    };
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string());
    validate_lock_status_reason(
        &status,
        commit,
        host,
        boot_id.as_deref(),
        &mut validate_runtime::identity_in_ancestry,
    )
}

/// Parse and bind one validation-lock authority response. The injected identity
/// predicate is the real `/proc` ancestry check in production and a planted,
/// inert identity in `--self-test`; no caller-supplied environment marker can
/// bypass these exact commit, host, boot, PID, and start-time checks.
fn validate_lock_status_admits(
    status: &[u8],
    commit: &str,
    host: &str,
    boot_id: Option<&str>,
    identity_in_ancestry: &mut dyn FnMut(i32, u64) -> bool,
) -> bool {
    validate_lock_status_reason(status, commit, host, boot_id, identity_in_ancestry)
        .is_ok()
}

/// The single implementation of the admission decision, reporting WHICH
/// conjunct failed.
///
/// ⚠️ SIXTEEN DISTINCT WAYS TO BE REFUSED USED TO COLLAPSE INTO ONE `false`,
/// and the front door then printed one sentence naming all three of exact
/// commit, exact host and live owner ancestry without saying which had failed
/// or what the values were. That is undiagnosable from the outside: the owner
/// hit it from his own checkout and could not tell that his HEAD simply was not
/// the commit his lock was taken for. Measured 2026-08-25 -- checkout HEAD
/// `b120fe5d7653`, lock target `1d558d48b438`.
///
/// The refusal was CORRECT. Validating `b120fe5d` while the receipt says
/// `1d558d48` would record a result against a commit it was not measured on,
/// which is the exact defect the target binding exists to prevent. So nothing
/// here is relaxed and no caller is exempted -- `..._admits` is derived from
/// this function, so the decision cannot drift from the explanation. Only the
/// diagnosis is added.
/// ⚠️ `identity_in_ancestry` IS A TRAIT OBJECT, NOT `impl FnMut`, AND IT HAS TO
/// STAY ONE. This function RECURSES over the `authorities` array and passes
/// `&mut identity_in_ancestry` down, so with a generic parameter each level
/// instantiates one more reference layer -- `F`, `&mut F`, `&mut &mut F`, ...
/// -- and monomorphization never terminates. Measured 2026-08-26: as
/// `impl FnMut` this failed to compile with "reached the recursion limit while
/// instantiating `validate_lock_status_reason::<&mut &mut &mut &mut
/// &mut ...>`", which took the whole validate gate off the air on `main` --
/// validate could not build, so nothing could be validated at all.
///
/// ⚠️ IT DID NOT SHOW UP IN `--self-test`. `rust-script --test` builds a
/// different crate configuration and passed 16/16 while the release build the
/// gate actually runs could not compile. A green self-test is therefore NOT
/// evidence that this file builds; only a release build is.
fn validate_lock_status_reason(
    status: &[u8],
    commit: &str,
    host: &str,
    boot_id: Option<&str>,
    identity_in_ancestry: &mut dyn FnMut(i32, u64) -> bool,
) -> Result<(), String> {
    fn object_string<'a>(
        object: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<&'a str> {
        object.get(key).and_then(serde_json::Value::as_str)
    }
    fn shown(value: Option<&str>) -> String {
        value.map_or_else(|| "<absent>".to_string(), |text| format!("{text:?}"))
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(status) else {
        return Err("the authority response is not valid JSON".into());
    };
    if let Some(authorities) = value
        .get("authorities")
        .and_then(serde_json::Value::as_array)
    {
        if authorities.is_empty() {
            return Err("the authority response contains no validation slots".into());
        }
        let mut reasons = Vec::new();
        for authority in authorities {
            let encoded = serde_json::to_vec(authority)
                .map_err(|error| format!("cannot encode validation slot: {error}"))?;
            match validate_lock_status_reason(
                &encoded,
                commit,
                host,
                boot_id,
                &mut *identity_in_ancestry,
            ) {
                Ok(()) => return Ok(()),
                Err(reason) => reasons.push(reason),
            }
        }
        return Err(format!(
            "none of the canonical validation slots belongs to this run: {}",
            reasons.join("; ")
        ));
    }
    let Some(holder) = value.get("holder").and_then(serde_json::Value::as_object) else {
        return Err(format!(
            "no lock is held: the authority reports state {} (reason {}), so there \
             is no holder to bind to",
            shown(value.get("state").and_then(serde_json::Value::as_str)),
            shown(value.get("reason_code").and_then(serde_json::Value::as_str)),
        ));
    };
    let Some(owner) = value.get("owner").and_then(serde_json::Value::as_object) else {
        return Err("the authority reports a holder but no owner record".into());
    };
    if value.get("schema_version").and_then(serde_json::Value::as_i64) != Some(1)
    {
        return Err("the authority response is not schema_version 1".into());
    }
    let holder_kind = object_string(holder, "kind");
    match holder_kind {
        Some("validate") => {
            if value.get("admissible").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err("the authority itself reports admissible=false".into());
            }
            if !value
                .get("reason_code")
                .is_some_and(serde_json::Value::is_null)
            {
                return Err(format!(
                    "the authority attached reason_code {}",
                    shown(value.get("reason_code").and_then(serde_json::Value::as_str))
                ));
            }
        }
        Some("frozen-validate") => {
            if value
                .get("lock_admissible")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            {
                return Err("the frozen authority itself reports lock_admissible=false".into());
            }
            if value.get("admissible").and_then(serde_json::Value::as_bool) != Some(false) {
                return Err(
                    "the frozen authority must remain noncanonical (admissible=false)".into(),
                );
            }
            if value.get("reason_code").and_then(serde_json::Value::as_str)
                != Some("canonical-holder-kind-not-validate")
            {
                return Err(format!(
                    "the frozen authority attached reason_code {}, not \
                     \"canonical-holder-kind-not-validate\"",
                    shown(value.get("reason_code").and_then(serde_json::Value::as_str))
                ));
            }
        }
        _ => {
            return Err(format!(
                "the held lock is kind {}, not \"validate\" or \"frozen-validate\"",
                shown(holder_kind)
            ));
        }
    }
    if value.get("state").and_then(serde_json::Value::as_str) != Some("held") {
        return Err(format!(
            "the lock state is {}, not \"held\"",
            shown(value.get("state").and_then(serde_json::Value::as_str))
        ));
    }
    if value
        .get("canonical_anchor_held")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err("the canonical anchor is not held".into());
    }
    if !matches!(
        value
            .get("cleanup_state")
            .and_then(serde_json::Value::as_str),
        Some("none" | "active-bound")
    ) {
        return Err(format!(
            "cleanup_state is {}, which is neither \"none\" nor \"active-bound\"",
            shown(
                value
                    .get("cleanup_state")
                    .and_then(serde_json::Value::as_str)
            )
        ));
    }
    if object_string(holder, "target") != Some(commit) {
        return Err(format!(
            "COMMIT MISMATCH: this checkout is at {commit}, but the lock was taken \
             for target {}. Validating here would measure {commit} and record it \
             against the lock's target, so it is refused. Check out the lock's \
             target, or take a lock for this commit.",
            shown(object_string(holder, "target"))
        ));
    }
    if object_string(holder, "host") != Some(host) {
        return Err(format!(
            "HOST MISMATCH: this host is {host:?}, the lock holder's host is {}",
            shown(object_string(holder, "host"))
        ));
    }
    if object_string(owner, "host") != Some(host) {
        return Err(format!(
            "HOST MISMATCH: this host is {host:?}, the lock owner's host is {}",
            shown(object_string(owner, "host"))
        ));
    }
    if object_string(owner, "liveness") != Some("alive") {
        return Err(format!(
            "the lock owner's liveness is {}, not \"alive\"",
            shown(object_string(owner, "liveness"))
        ));
    }
    let Some(pid64) = owner.get("pid").and_then(serde_json::Value::as_i64) else {
        return Err("the lock owner record carries no integer pid".into());
    };
    let Some(start_ticks) = owner.get("start_ticks").and_then(serde_json::Value::as_u64) else {
        return Err("the lock owner record carries no start_ticks".into());
    };
    let Ok(pid) = i32::try_from(pid64) else {
        return Err(format!("the lock owner pid {pid64} is out of range"));
    };
    if pid <= 1 || start_ticks == 0 {
        return Err(format!(
            "the lock owner identity is degenerate (pid {pid}, start_ticks {start_ticks})"
        ));
    }
    if boot_id != object_string(owner, "boot_id") {
        return Err(format!(
            "BOOT MISMATCH: this boot is {}, the lock was taken under boot {}. The lock did not \
             survive a reboot.",
            shown(boot_id),
            shown(object_string(owner, "boot_id"))
        ));
    }
    if !identity_in_ancestry(pid, start_ticks) {
        return Err(format!(
            "ANCESTRY: this process is not a descendant of the lock owner (pid {pid}, start_ticks \
             {start_ticks}). A naked run is refused here even when a lock is held by someone else \
             -- enter through ci-hub so the run is a child of the lock owner."
        ));
    }
    Ok(())
}

/// Inert two-sided bracket for the front door and the validation-lock authority
/// parser. It proves the guard neither accepts missing/mismatched authority
/// nor mistakes a generic superproject for dev-hermit. Nested payloads remain
/// subject to the same authority, so their caller-supplied marker cannot become
/// an admission bypass.
fn product_front_door_bracket() -> Result<(), String> {
    let policy_cases = [
        (true, true, false, false, true, "dev-hermit top-level product run"),
        (false, false, false, false, false, "standalone clone"),
        (true, false, false, false, false, "generic Hermit superproject"),
        (true, true, true, false, true, "nested focused payload"),
        (true, true, false, true, false, "show-plan"),
    ];
    for (parent, ci_hub_dir, nested, show_plan, expected, label) in policy_cases {
        let actual = product_front_door_applies(parent, ci_hub_dir, nested, show_plan);
        if actual != expected {
            return Err(format!(
                "product front door classified {label} as applies={actual}, expected {expected}"
            ));
        }
    }

    let commit = "0123456789abcdef0123456789abcdef01234567";
    // Synthetic like the commit and boot_id above it. A real machine name here
    // is inert today but is how a fixture turns into a host dependency, and
    // scripts/check-portable-paths.sh refuses literal hostnames in tracked
    // build/run files for exactly that reason.
    let host = "test-host-0";
    let boot_id = "11111111-2222-3333-4444-555555555555";
    let authority = serde_json::json!({
        "schema_version": 1,
        "lock_admissible": true,
        "admissible": true,
        "state": "held",
        "reason_code": null,
        "canonical_anchor_held": true,
        "cleanup_state": "active-bound",
        "holder": {"kind": "validate", "target": commit, "host": host},
        "owner": {
            "host": host,
            "liveness": "alive",
            "pid": 4242,
            "start_ticks": 987654,
            "boot_id": boot_id
        }
    });
    let encode = |value: &serde_json::Value| serde_json::to_vec(value).unwrap();
    if !validate_lock_status_admits(
        &encode(&authority),
        commit,
        host,
        Some(boot_id),
        &mut (|pid, ticks| pid == 4242 && ticks == 987654),
    ) {
        return Err("product front door refused exact canonical authority".into());
    }

    let mut frozen_authority = authority.clone();
    frozen_authority["lock_admissible"] = serde_json::json!(true);
    frozen_authority["admissible"] = serde_json::json!(false);
    frozen_authority["reason_code"] =
        serde_json::json!("canonical-holder-kind-not-validate");
    frozen_authority["holder"]["kind"] = serde_json::json!("frozen-validate");
    if !validate_lock_status_admits(
        &encode(&frozen_authority),
        commit,
        host,
        Some(boot_id),
        &mut (|pid, ticks| pid == 4242 && ticks == 987654),
    ) {
        return Err("product front door refused exact frozen authority".into());
    }

    let mut frozen_weakened = Vec::new();
    let mut value = frozen_authority.clone();
    value["lock_admissible"] = serde_json::json!(false);
    frozen_weakened.push(("not lock-admissible", value));
    let mut value = frozen_authority.clone();
    value["holder"]["target"] = serde_json::json!("different-commit");
    frozen_weakened.push(("wrong target", value));
    let mut value = frozen_authority.clone();
    value["owner"]["liveness"] = serde_json::json!("dead");
    frozen_weakened.push(("owner not alive", value));
    for (label, value) in frozen_weakened {
        if validate_lock_status_admits(
            &encode(&value),
            commit,
            host,
            Some(boot_id),
            &mut (|pid, ticks| pid == 4242 && ticks == 987654),
        ) {
            return Err(format!(
                "product front door accepted invalid frozen authority: {label}"
            ));
        }
    }

    // Goalpost safety: every case below weakens or changes an identity claim.
    // Each must remain non-authorizing; improving diagnostics must never turn
    // one into an exemption.
    let mut weakened = Vec::new();
    let mut value = authority.clone();
    value["schema_version"] = serde_json::json!(2);
    weakened.push(("wrong schema", value));
    let mut value = authority.clone();
    value["admissible"] = serde_json::json!(false);
    weakened.push(("not admissible", value));
    let mut value = authority.clone();
    value["state"] = serde_json::json!("free");
    weakened.push(("lock not held", value));
    let mut value = authority.clone();
    value["reason_code"] = serde_json::json!("owner-not-ancestor");
    weakened.push(("non-null refusal reason", value));
    let mut value = authority.clone();
    value["canonical_anchor_held"] = serde_json::json!(false);
    weakened.push(("canonical anchor absent", value));
    let mut value = authority.clone();
    value["cleanup_state"] = serde_json::json!("stale");
    weakened.push(("invalid cleanup state", value));
    let mut value = authority.clone();
    value["holder"]["kind"] = serde_json::json!("other");
    weakened.push(("wrong holder kind", value));
    let mut value = authority.clone();
    value["holder"]["target"] = serde_json::json!("different-commit");
    weakened.push(("wrong commit", value));
    let mut value = authority.clone();
    value["holder"]["host"] = serde_json::json!("other-host");
    weakened.push(("wrong holder host", value));
    let mut value = authority.clone();
    value["owner"]["host"] = serde_json::json!("other-host");
    weakened.push(("wrong owner host", value));
    let mut value = authority.clone();
    value["owner"]["liveness"] = serde_json::json!("dead");
    weakened.push(("owner not alive", value));
    let mut value = authority.clone();
    value["owner"]["pid"] = serde_json::json!(1);
    weakened.push(("unsafe owner pid", value));
    let mut value = authority.clone();
    value["owner"]["start_ticks"] = serde_json::json!(0);
    weakened.push(("zero owner start ticks", value));
    let mut value = authority.clone();
    value["owner"]["boot_id"] = serde_json::json!("other-boot");
    weakened.push(("wrong boot", value));
    for (label, value) in weakened {
        if validate_lock_status_admits(
            &encode(&value),
            commit,
            host,
            Some(boot_id),
            &mut (|pid, ticks| pid == 4242 && ticks == 987654),
        ) {
            return Err(format!("product front door accepted weakened authority: {label}"));
        }
    }
    if validate_lock_status_admits(
        &encode(&authority),
        commit,
        host,
        Some(boot_id),
        &mut (|_pid, _ticks| false),
    ) {
        return Err("product front door accepted authority outside owner ancestry".into());
    }

    // These were historical, forgeable authorization inputs. The canonical
    // parser is deliberately pure with respect to the process environment; pin
    // that property against all legacy spellings still present in old tests.
    let legacy_env = [
        ("CI_HUB_VALIDATE_PRODUCER", "forged"),
        ("CI_HUB_VALIDATE_LOCK_OWNER_PID", "4242"),
        ("CI_HUB_VALIDATE_LOCK_OWNER_FILE", "/tmp/forged-owner"),
    ];
    let saved = legacy_env.map(|(name, _)| (name, std::env::var_os(name)));
    // SAFETY: this self-test is single-threaded at this point, and every value
    // is restored before returning from the bracket.
    for (name, value) in legacy_env {
        unsafe { std::env::set_var(name, value) };
    }
    let forged_env_admitted = validate_lock_status_admits(
        br#"{"schema_version":1,"admissible":false}"#,
        commit,
        host,
        Some(boot_id),
        &mut (|_pid, _ticks| true),
    );
    for (name, value) in saved {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
    if forged_env_admitted {
        return Err("product front door trusted forged legacy owner environment".into());
    }

    let parent = PathBuf::from("/srv/dev-hermit");
    let checkout = parent.join("worktrees/slot07/hermit");
    let refusal = product_front_door_refusal(
        &parent,
        &checkout,
        commit,
        "--strict-compat-only",
        true,
        false,
    )
    .ok_or_else(|| "product front door omitted the naked-run refusal".to_string())?;
    if !refusal.contains("Publishing because the code is ready requires ci-hub")
        || !refusal.contains(commit)
        || !refusal.contains("ci-hub/ci-hub validate-run")
        || !refusal.contains(ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION)
        || !refusal.contains("--only portable test.cli")
        || !refusal.contains("cannot be cited as validation evidence")
    {
        return Err(format!("product front-door refusal lost remediation detail: {refusal}"));
    }
    if product_front_door_refusal(
        &parent,
        &checkout,
        commit,
        "--strict-compat-only",
        true,
        true,
    )
    .is_some()
    {
        return Err("product front door refused canonical admission".into());
    }

    let unavailable = product_front_door_refusal(
        &parent,
        &checkout,
        commit,
        "--strict-compat-only",
        false,
        false,
    )
    .ok_or_else(|| "product front door omitted the missing-launcher refusal".to_string())?;
    if !unavailable.contains("launcher is unavailable")
        || unavailable.contains("validate-run --checkout")
    {
        return Err(format!("missing-launcher refusal printed a false remedy: {unavailable}"));
    }

    let focused = parse_argv(&[
        ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into(),
        "--only".into(),
        "portable".into(),
        "test.cli".into(),
    ])
    .map_err(|code| format!("off-the-record focused form did not parse: exit {code}"))?;
    if focused.label_pr
        || local_off_the_record_refusal(&focused, false).is_some()
        || !local_off_the_record_refusal(&focused, true)
            .is_some_and(|message| message.contains("Commit the work in progress first"))
    {
        return Err(
            "off-the-record focused form did not disable publication or enforce a clean commit"
                .into(),
        );
    }

    let full = parse_argv(&[ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into(), "full".into()])
        .map_err(|code| format!("off-the-record full form did not parse: exit {code}"))?;
    let full_refusal = local_off_the_record_refusal(&full, false)
        .ok_or_else(|| "off-the-record full run was not refused".to_string())?;
    if !full_refusal.contains("full-cost validate belongs in ci-hub")
        || !full_refusal.contains("--only portable test.cli")
    {
        return Err(format!(
            "off-the-record full refusal lost required guidance: {full_refusal}"
        ));
    }

    let quick = parse_argv(&[ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into(), "quick".into()])
        .map_err(|code| format!("off-the-record quick form did not parse: exit {code}"))?;
    if local_off_the_record_refusal(&quick, false).is_some() {
        return Err("off-the-record quick run was incorrectly refused".into());
    }

    println!(
        "  product front door: publishing requires authority; clean quick/focused local iteration \
         is off the record; dirty/full local forms and diagnostics bracketed"
    );
    Ok(())
}

/// Drive this exact executable through the real `run()` entry path. The pure
/// bracket above pins policy details; this process bracket pins the wiring and
/// proves full, focused, and caller-marked nested work all stop before creating
/// validation state when canonical authority is missing or malformed.
fn product_front_door_process_bracket() -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("front-door process bracket: current executable: {error}"))?;
    let git_dir = sh("git", &["rev-parse", "--absolute-git-dir"])
        .ok_or_else(|| "front-door process bracket: cannot resolve git dir".to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("front-door process bracket: system clock: {error}"))?
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "validate-front-door-process-{}-{nonce}",
        std::process::id()
    ));

    let result = (|| {
        let cases: [(&str, &[&str], bool, bool, &str); 3] = [
            (
                "top-level-missing-launcher",
                &[
                    "full",
                    SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                ],
                false,
                false,
                "launcher is unavailable",
            ),
            (
                "focused-invalid-authority",
                &[
                    "--strict-compat-only",
                    SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                ],
                false,
                true,
                "Publishing because the code is ready requires ci-hub",
            ),
            (
                "nested-marker-invalid-authority",
                &[
                    "--strict-compat-only",
                    SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                ],
                true,
                true,
                "Publishing because the code is ready requires ci-hub",
            ),
        ];

        for (label, args, nested, launcher_present, expected_remediation) in cases {
            let parent = tmp.join(label);
            let checkout = parent.join("hermit");
            let ci_hub_dir = parent.join("ci-hub");
            std::fs::create_dir_all(&checkout).map_err(|error| {
                format!(
                    "front-door process bracket: cannot create {}: {error}",
                    checkout.display()
                )
            })?;
            std::fs::create_dir_all(&ci_hub_dir).map_err(|error| {
                format!(
                    "front-door process bracket: cannot create {}: {error}",
                    ci_hub_dir.display()
                )
            })?;
            std::fs::write(
                parent.join(".gitmodules"),
                "[submodule \"hermit\"]\n\tpath = hermit\n\turl = self-test://unused\n",
            )
            .map_err(|error| {
                format!("front-door process bracket: cannot write .gitmodules: {error}")
            })?;
            if launcher_present {
                let launcher = ci_hub_dir.join("ci-hub");
                std::fs::write(
                    &launcher,
                    "#!/bin/sh\nprintf '%s\\n' \
                     '{\"schema_version\":1,\"admissible\":false}'\n",
                )
                .map_err(|error| {
                    format!(
                        "front-door process bracket: cannot write {}: {error}",
                        launcher.display()
                    )
                })?;
                std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| {
                        format!(
                            "front-door process bracket: cannot chmod {}: {error}",
                            launcher.display()
                        )
                    })?;
            }

            let mut command = Command::new(&executable);
            command
                .args(args)
                .current_dir(&checkout)
                .env("GIT_DIR", &git_dir)
                .env("GIT_WORK_TREE", &checkout)
                .env_remove("HERMIT_VALIDATE_STOP_TEST_MODE")
                .env_remove("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON")
                .env_remove("HERMIT_VALIDATE_STOP_TEST_EXIT_EARLY")
                .env_remove(PARENT_ENV)
                .env_remove(TOOL_ROOT_ENV)
                .env_remove(validate_runtime::ACTIVE_ENV)
                .env_remove("CI_HUB_VALIDATE_LOCK_OWNER_PID")
                .env_remove("CI_HUB_VALIDATE_LOCK_OWNER_FILE");
            if nested {
                command
                    .env(validate_runtime::ACTIVE_ENV, std::process::id().to_string())
                    .env(E2E_MACHINE_SHORTNAME_ENV, "fixture-host")
                    .env(E2E_KERNEL_VERSION_ENV, "fixture-kernel")
                    .env("CI_HUB_VALIDATE_LOCK_OWNER_PID", std::process::id().to_string())
                    .env(
                        "CI_HUB_VALIDATE_LOCK_OWNER_FILE",
                        parent.join("caller-forged-owner"),
                    );
            }
            let output = command.output().map_err(|error| {
                format!("front-door process bracket: cannot launch {label}: {error}")
            })?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let rendered = format!(
                "{}{}",
                stdout,
                String::from_utf8_lossy(&output.stderr)
            );
            if output.status.code() != Some(i32::from(COULD_NOT_RUN_EXIT_CODE))
                || !rendered.contains("choose whether this is iterative testing or publishing")
                || !rendered.contains(expected_remediation)
                || !rendered.contains("--only portable test.cli")
                || stdout.lines().last()
                    != Some("FINAL_VALIDATE_STATUS: COULD_NOT_RUN")
            {
                return Err(format!(
                    "front-door process bracket: {label} escaped/refused incorrectly: status={:?} \
                     output={rendered}",
                    output.status.code()
                ));
            }
            for unexpected in [
                checkout.join("target/validation"),
                checkout.join("ignored/validate"),
                parent.join("ledger"),
            ] {
                if unexpected.exists() {
                    return Err(format!(
                        "front-door process bracket: {label} created side effect before refusal: {}",
                        unexpected.display()
                    ));
                }
            }
        }
        Ok(())
    })();

    let cleanup = std::fs::remove_dir_all(&tmp)
        .map_err(|error| format!("front-door process bracket: cannot remove {}: {error}", tmp.display()));
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            println!(
                "  product front door process: full/focused/nested missing-authority runs refused \
                 before validation state"
            );
            Ok(())
        }
        (Err(problem), Ok(())) => Err(problem),
        (Ok(()), Err(cleanup_problem)) => Err(cleanup_problem),
        (Err(problem), Err(cleanup_problem)) => {
            Err(format!("{problem}; cleanup also failed: {cleanup_problem}"))
        }
    }
}

/// Aggregate libtest `executed` / `filtered` counts from typed step outcomes.
///
/// **This is the field the whole receipt rests on.** A row whose
/// `executed_tests` is null is a NON-VERDICT: every downstream completeness
/// predicate keys `is_clean_full_pass` on a nonzero executed count, so a driver
/// that ran no tests at all would otherwise emit a row indistinguishable from one
/// that ran the whole suite. `main` at `61edbef4` recorded 862 executed / 693
/// filtered, and a port that cannot reproduce that number has not preserved the
/// thing validate exists to do.
///
/// The runner derives these values from each step's COMPLETE captured bytes
/// before verbosity filters presentation. Thus level 1 can stay O(steps)
/// without erasing the receipt's evidence. `None` remains UNKNOWN and `Some(0)`
/// remains a demonstrated vacuous run; neither is coerced.
fn sum_typed_count(
    outcomes: &[StepOutcome],
    select: fn(&StepOutcome) -> Option<u64>,
) -> Option<i64> {
    let mut seen = false;
    let mut total = 0u64;
    for outcome in outcomes {
        if let Some(value) = select(outcome) {
            seen = true;
            total = total.checked_add(value)?;
        }
    }
    seen.then(|| i64::try_from(total).ok()).flatten()
}

fn exact_passed_test_count(outcomes: &[StepOutcome]) -> Option<i64> {
    let mut seen = false;
    let mut passed = 0u64;
    for outcome in outcomes {
        let count_bearing = outcome.executed_tests.is_some()
            || outcome.filtered_tests.is_some()
            || outcome.test_results.is_some();
        if !count_bearing {
            continue;
        }
        seen = true;
        let executed = outcome.executed_tests?;
        let outcome_passed = match &outcome.test_results {
            Some(results) => {
                if u64::try_from(results.len()).ok()? != executed {
                    return None;
                }
                u64::try_from(results.iter().filter(|result| result.passed).count()).ok()?
            }
            None if executed == 0 || outcome.ok => executed,
            None => return None,
        };
        passed = passed.checked_add(outcome_passed)?;
    }
    seen.then(|| i64::try_from(passed).ok()).flatten()
}

fn libtest_counts(outcomes: &[StepOutcome]) -> (Option<i64>, Option<i64>, Option<i64>) {
    (
        sum_typed_count(outcomes, |o| o.executed_tests),
        exact_passed_test_count(outcomes),
        sum_typed_count(outcomes, |o| o.filtered_tests),
    )
}

fn compat_test_results(
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
    prefix: &str,
) -> Result<TestResults, String> {
    let compat_outcomes = outcomes
        .iter()
        .filter(|outcome| outcome.tag.starts_with(prefix))
        .map(|outcome| (outcome.tag.as_str(), outcome))
        .collect::<BTreeMap<_, _>>();
    let mut latest_attempts = BTreeMap::<&str, &NodeAttempt>::new();
    for attempt in attempts
        .iter()
        .filter(|attempt| attempt.tag.starts_with(prefix))
    {
        let latest = latest_attempts
            .entry(attempt.tag.as_str())
            .or_insert(attempt);
        if attempt.attempt > latest.attempt {
            *latest = attempt;
        }
    }
    for (tag, latest) in &latest_attempts {
        if !compat_outcomes.contains_key(tag) {
            return Err(format!(
                "structured compatibility result {tag} has attempt {} but no retained outcome",
                latest.attempt
            ));
        }
    }

    let mut results = Vec::new();
    for outcome in outcomes {
        let Some(label) = outcome.tag.strip_prefix(prefix) else {
            continue;
        };
        let tag = outcome.tag.as_str();
        let latest = latest_attempts.get(tag).copied().ok_or_else(|| {
            format!("structured compatibility result {label} has no recorded attempt")
        })?;
        if !latest.reported || latest.execution != AttemptExecution::Completed {
            return Err(format!(
                "structured compatibility result {label} latest attempt {} has no completed report",
                latest.attempt
            ));
        }
        let retained_result = if outcome_execution(outcome) != AttemptExecution::Completed {
            None
        } else if outcome_is_no_result(outcome) {
            Some("no_result")
        } else if outcome.ok {
            Some("pass")
        } else {
            Some("fail")
        };
        let latest_result = attempt_result(latest);
        if latest_result != retained_result || latest.returncode != outcome.returncode {
            return Err(format!(
                "structured compatibility result {label} disagrees with latest attempt {}",
                latest.attempt
            ));
        }
        let passed = match latest_result {
            Some("pass") => true,
            Some("fail") => false,
            _ => {
                return Err(format!(
                    "structured compatibility result {label} latest attempt {} has no pass/fail verdict",
                    latest.attempt
                ));
            }
        };
        let attempt_count = u64::try_from(latest.attempt).map_err(|_| {
            format!("structured compatibility attempts overflowed for {label}")
        })?;
        results.push(TestResult::new(label.to_string(), passed, attempt_count)?);
    }
    let executed = u64::try_from(results.len())
        .map_err(|_| "structured compatibility result count does not fit u64".to_string())?;
    TestResults::current(executed, 0, results)
}

/// Add the direct compatibility rows to the exact test denominator.
///
/// Before strict compatibility was flattened, its nested validate published a
/// single structured-count file to the outer `test.strict_compat` step. The
/// direct `compat.*` steps already carry stronger typed terminal outcomes and
/// attempts, so the outer producer now consumes those facts itself. A failed
/// count-bearing non-compatibility node still leaves the passed count unknown;
/// flattening must not turn an inexact base count into an exact-looking total.
fn run_test_counts(
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
    compat: Option<CompatMode>,
    compat_prefix: Option<&str>,
) -> Result<(Option<i64>, Option<i64>, Option<i64>), String> {
    let (base_executed, base_passed, base_filtered) = libtest_counts(outcomes);
    if compat.is_none() {
        return Ok((base_executed, base_passed, base_filtered));
    }

    let prefix = compat_prefix.ok_or("compatibility mode has no committed tag prefix")?;
    let compatibility = compat_test_results(outcomes, attempts, prefix)?;
    let compat_executed = i64::try_from(compatibility.executed_tests)
        .map_err(|_| "structured compatibility executed count does not fit i64".to_string())?;
    let compat_filtered = i64::try_from(compatibility.filtered_tests)
        .map_err(|_| "structured compatibility filtered count does not fit i64".to_string())?;
    let compat_passed = compatibility
        .results
        .as_ref()
        .ok_or("structured compatibility producer retained count-only schema")?
        .iter()
        .filter(|result| result.passed)
        .count();
    let compat_passed = i64::try_from(compat_passed)
        .map_err(|_| "structured compatibility passed count does not fit i64".to_string())?;
    let add = |left: i64, right: i64, name: &str| {
        left.checked_add(right)
            .ok_or_else(|| format!("combined {name} test count overflowed i64"))
    };

    let executed = Some(match base_executed {
        Some(base) => add(base, compat_executed, "executed")?,
        None => compat_executed,
    });
    let passed = match (base_executed, base_passed) {
        (None, _) => Some(compat_passed),
        (Some(_), Some(base)) => Some(add(base, compat_passed, "passed")?),
        (Some(_), None) => None,
    };
    let filtered = Some(match base_filtered {
        Some(base) => add(base, compat_filtered, "filtered")?,
        None => compat_filtered,
    });
    Ok((executed, passed, filtered))
}

/// Derive the per-node coverage obligation from dagrun's structured test counts.
///
/// A terminal node with no structured count file has no executed-test evidence.
/// It belongs in `absent_nodes` even if its captured stdout contains a line that
/// resembles libtest output. `Some(0)` remains the distinct, demonstrated
/// zero-execution state. Only a positive producer-written count satisfies one
/// planned `test.*` node.
fn typed_test_node_coverage(
    planned_test_nodes: &BTreeSet<String>,
    outcomes: &[StepOutcome],
) -> serde_json::Value {
    let final_outcomes: BTreeMap<&str, &StepOutcome> =
        outcomes.iter().map(|outcome| (outcome.tag.as_str(), outcome)).collect();
    let mut executed = 0usize;
    let mut zero_executed_nodes = Vec::new();
    let mut absent_nodes = Vec::new();
    for tag in planned_test_nodes {
        match final_outcomes.get(tag.as_str()) {
            Some(outcome) if !outcome.aborted && outcome.executed_tests.is_some_and(|n| n > 0) => {
                executed += 1;
            }
            Some(outcome) if !outcome.aborted && outcome.executed_tests == Some(0) => {
                zero_executed_nodes.push(tag.clone());
            }
            _ => absent_nodes.push(tag.clone()),
        }
    }
    serde_json::json!({
        "planned_test_nodes": planned_test_nodes.len(),
        "executed_test_nodes": executed,
        "zero_executed_nodes": zero_executed_nodes,
        "absent_nodes": absent_nodes,
    })
}

/// Two-sided bracket for the structured per-node coverage judgement.
fn test_node_coverage_bracket() -> Result<(), String> {
    let outcome = |tag: &str, ok: bool, aborted: bool, executed_tests| StepOutcome {
        tag: tag.into(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests,
        filtered_tests: Some(0),
        test_results: None,
        returncode: Some(if ok { 0 } else { 100 }),
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: if ok { String::new() } else { "test failure".into() },
        aborted,
    };

    let planned = BTreeSet::from([
        "test.aborted".to_string(),
        "test.banner_only".to_string(),
        "test.missing".to_string(),
        "test.ran_failed".to_string(),
        "test.ran_passed".to_string(),
        "test.zero".to_string(),
    ]);
    let outcomes = vec![
        outcome("test.aborted", false, true, Some(9)),
        outcome("test.banner_only", true, false, None),
        outcome("test.ran_failed", false, false, Some(23)),
        outcome("test.ran_passed", true, false, Some(17)),
        outcome("test.zero", true, false, Some(0)),
        outcome("test.unplanned", true, false, Some(99)),
    ];
    let coverage = typed_test_node_coverage(&planned, &outcomes);
    let expected = serde_json::json!({
        "planned_test_nodes": 6,
        "executed_test_nodes": 2,
        "zero_executed_nodes": ["test.zero"],
        "absent_nodes": ["test.aborted", "test.banner_only", "test.missing"],
    });
    if coverage != expected {
        return Err(format!(
            "test-node coverage: structured outcome classification disagrees: {coverage}"
        ));
    }

    println!(
        "  test-node coverage: 2 structured-positive / 1 structured-zero / 3 absent; printed banners alone remain absent"
    );
    Ok(())
}

fn typed_libtest_count_bracket() -> Result<(), String> {
    let outcome = |tag: &str, ok: bool, executed_tests, filtered_tests| StepOutcome {
        tag: tag.into(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests,
        filtered_tests,
        test_results: None,
        returncode: Some(if ok { 0 } else { 100 }),
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: if ok { String::new() } else { "test failure".into() },
        aborted: false,
    };
    let full = vec![
        outcome("test.a", true, Some(398), Some(0)),
        outcome("test.b", true, Some(475), Some(350)),
    ];
    if libtest_counts(&full) != (Some(873), Some(873), Some(350)) {
        return Err("typed libtest counts: complete outcomes did not sum to 873/873/350".into());
    }
    let failed = outcome("test.failed", false, Some(23), Some(5));
    if libtest_counts(std::slice::from_ref(&failed)) != (Some(23), None, Some(5)) || failed.ok {
        return Err(
            "typed libtest counts: a retained failed count-only outcome must keep passed unknown"
                .into(),
        );
    }
    if libtest_counts(&[outcome("test.zero", true, Some(0), Some(0))])
        != (Some(0), Some(0), Some(0))
    {
        return Err("typed libtest counts: demonstrated zero was not preserved".into());
    }
    if libtest_counts(&[outcome("build.only", true, None, None)]) != (None, None, None) {
        return Err("typed libtest counts: unknown bannerless output was coerced".into());
    }

    let mut exact = outcome("test.exact", false, Some(2), Some(0));
    exact.test_results = Some(vec![
        TestResult::new("case-a".into(), true, 1)?,
        TestResult::new("case-b".into(), false, 2)?,
    ]);
    if libtest_counts(std::slice::from_ref(&exact)) != (Some(2), Some(1), Some(0)) {
        return Err("typed libtest counts: exact per-test results did not report one pass".into());
    }
    exact.test_results.as_mut().unwrap()[1].passed = true;
    if libtest_counts(std::slice::from_ref(&exact)) != (Some(2), Some(2), Some(0)) {
        return Err(
            "typed libtest counts: mutating a typed terminal result did not move the pass count"
                .into(),
        );
    }

    let mut mixed_failure = outcome("test.mixed-failure", false, Some(2), Some(1));
    mixed_failure.test_results = Some(vec![
        TestResult::new("mixed-pass".into(), true, 1)?,
        TestResult::new("mixed-fail".into(), false, 1)?,
    ]);
    let successful_count_only = outcome("test.successful-count-only", true, Some(3), Some(2));
    if libtest_counts(&[mixed_failure, successful_count_only])
        != (Some(5), Some(4), Some(3))
    {
        return Err(
            "typed libtest counts: mixed failed typed and successful count-only outcomes did not retain an exact pass total"
                .into(),
        );
    }

    let compat_pass = outcome("compat.pass-case", true, None, None);
    let compat_fail = outcome("compat.fail-case", false, None, None);
    let compat_attempts = vec![
        reported_attempt(&compat_pass, 1),
        reported_attempt(&compat_fail, 1),
        reported_attempt(&compat_fail, 2),
    ];
    let compat = compat_test_results(
        &[compat_pass.clone(), compat_fail.clone()],
        &compat_attempts,
        "compat.",
    )?;
    let compat_rows = compat
        .results
        .as_ref()
        .ok_or("typed libtest counts: compatibility producer wrote retained schema 1")?;
    if compat.executed_tests != 2
        || compat.filtered_tests != 0
        || compat_rows
            != &vec![
                TestResult::new("pass-case".into(), true, 1)?,
                TestResult::new("fail-case".into(), false, 2)?,
            ]
    {
        return Err(format!(
            "typed libtest counts: compatibility results lost a verdict or attempt: {compat:?}"
        ));
    }
    if run_test_counts(
        &[
            full[0].clone(),
            full[1].clone(),
            compat_pass.clone(),
            compat_fail.clone(),
        ],
        &compat_attempts,
        Some(CompatMode::PortableStrict),
        Some("compat."),
    )? != (Some(875), Some(874), Some(350))
    {
        return Err(
            "typed libtest counts: direct compatibility rows did not join the exact outer total"
                .into(),
        );
    }
    if run_test_counts(
        &[compat_pass.clone(), compat_fail.clone()],
        &compat_attempts,
        Some(CompatMode::PortableStrict),
        Some("compat."),
    )? != (Some(2), Some(1), Some(0))
    {
        return Err(
            "typed libtest counts: compatibility-only run did not retain an exact total".into(),
        );
    }
    if run_test_counts(
        &[failed, compat_pass.clone(), compat_fail.clone()],
        &compat_attempts,
        Some(CompatMode::PortableStrict),
        Some("compat."),
    )? != (Some(25), None, Some(5))
    {
        return Err(
            "typed libtest counts: direct compatibility rows hid an inexact failed base count"
                .into(),
        );
    }
    let missing_attempt = compat_test_results(&[compat_pass], &[], "compat.")
        .expect_err("compatibility result without an attempt must refuse");
    if !missing_attempt.contains("pass-case") || !missing_attempt.contains("no recorded attempt")
    {
        return Err(format!(
            "typed libtest counts: missing compatibility attempt did not fail by name: {missing_attempt}"
        ));
    }

    let stale_fail = outcome("compat.stale-fail", false, None, None);
    let stale_attempts = vec![
        reported_attempt(&stale_fail, 1),
        unreported_attempt(stale_fail.tag.clone(), 2),
    ];
    let stale_error = compat_test_results(&[stale_fail], &stale_attempts, "compat.")
        .expect_err("a fail followed by an unreported retry must refuse");
    if !stale_error.contains("stale-fail")
        || !stale_error.contains("latest attempt 2")
        || !stale_error.contains("no completed report")
    {
        return Err(format!(
            "typed libtest counts: stale retained failure was not refused by latest attempt: {stale_error}"
        ));
    }
    println!(
        "  typed libtest counts: exact 873/873/350 pass; retained count-only failure stayed unknown; mixed typed failure aggregated exactly; typed mutation moved 1 -> 2; direct compatibility rows joined the outer denominator without hiding an unknown pass count; compatibility rows carried terminal verdicts and latest attempt ordinals; fail-then-unreported refused; 0/0/0 preserved"
    );
    Ok(())
}

fn set_gate_failure_evidence(gate: &mut serde_json::Value, failed: bool) {
    gate["failure_origin"] = serde_json::json!(failed.then_some("outer_gate"));
    if failed {
        // This producer schedules the named outer DAG node directly, so an
        // atomic failure positively has zero failed lane substeps.
        gate["failed_substeps"] = serde_json::json!([]);
    } else {
        // Absence means no failure evidence applies. Never serialize an unknown
        // collection as null: the typed reader correctly rejects that shape.
        gate.as_object_mut()
            .expect("ledger gate must remain a JSON object")
            .remove("failed_substeps");
    }
}

fn ledger_gate(outcome: &StepOutcome) -> serde_json::Value {
    let mut gate = serde_json::json!({
        "name": outcome.tag,
        "result": ledger_gate_result(outcome),
        "exit_code": outcome.returncode,
        "oomed": outcome.oomed,
        "oom_kills": outcome.oom_kills,
        "timed_out": outcome.timed_out,
        "cpu_timed_out": outcome.cpu_timed_out,
        "reason": outcome.reason,
        "aborted": outcome.aborted,
        "real_seconds": outcome.duration_s,
    });
    if let Some(failure_class) = outcome_failure_class(outcome) {
        gate["failure_class"] = serde_json::json!(failure_class);
        if failure_class == FailureClass::NoResult && !outcome.reason.is_empty() {
            gate["failure_detail"] = serde_json::json!(outcome.reason);
        }
    }
    set_gate_failure_evidence(&mut gate, outcome_is_failure(outcome));
    gate
}

/// Serialize the aggregate gate result alongside its latest raw observation.
///
/// Historical multiple-attempt rows can have a stale cumulative outcome or a
/// latest UNKNOWN attempt. The aggregate failure_class/result agree with the
/// run's fold; raw_result/raw_failure_class/raw_aborted and the unchanged attempt
/// list retain what was actually observed. No raw timeout/OOM fact is synthesized.
fn ledger_gate_with_attempts(outcome: &StepOutcome, attempts: &[NodeAttempt]) -> serde_json::Value {
    let node_attempts_raw: Vec<&NodeAttempt> =
        attempts.iter().filter(|attempt| attempt.tag == outcome.tag).collect();
    let first = node_attempts_raw.first().copied();
    let latest = terminal_attempt(outcome, attempts);
    let node_attempts: Vec<serde_json::Value> = node_attempts_raw
        .iter()
        .map(|a| {
            let assessment = environmental_assessment(attempts, a);
            let environmental_verdict = assessment.map(|(verdict, _)| verdict.as_str());
            let environmental_refuted_shape = assessment
                .and_then(|(_, shape)| shape)
                .map(validate_runtime::RefutedShape::as_str);
            let mut attempt = serde_json::json!({
                "attempt": a.attempt,
                // `null` is UNKNOWN and stays UNKNOWN: no completion payload
                // arrived, which is not the same as a failure and must never be
                // readable as a pass.
                "result": attempt_result(a),
                "understood_infrastructure_class": a.understood_infrastructure_class,
                "reported": a.reported,
                "execution": a.execution.as_str(),
                "exit_code": a.returncode,
                "oomed": a.oomed,
                "oom_kills": a.oom_kills,
                "timed_out": a.timed_out,
                "cpu_timed_out": a.cpu_timed_out,
                "reason": a.reason,
                "aborted": a.aborted,
                "real_seconds": a.reported.then_some(a.duration_s),
                // Why this attempt was given another go. `null` on the last
                // attempt of every node, since nothing followed it.
                "retry_class": a.retry_class.map(RetryClass::as_str),
                "retry_detail": a.retry_detail,
                // Classification is only a hypothesis. These fields say whether
                // a later actual execution confirmed/refuted it, or whether no
                // such execution occurred.
                "environmental_class": a.environmental_class,
                "environmental_detail_observed": a.detail_observed,
                "environmental_verdict": environmental_verdict,
                "environmental_refuted_shape": environmental_refuted_shape,
            });
            if let Some(failure_class) = a.failure_class {
                attempt["failure_class"] = serde_json::json!(failure_class);
            }
            if let Some(failure_detail) = &a.failure_detail {
                attempt["failure_detail"] = serde_json::json!(failure_detail);
            }
            attempt
        })
        .collect();

    // Synthetic stop-path fixtures predate attempt capture and intentionally pass
    // an empty ledger. Preserve their typed StepOutcome fallback; every scheduler
    // lane supplies attempts and therefore takes the exact-attempt branch.
    let mut gate = ledger_gate(outcome);
    gate["reported"] = serde_json::json!(latest.map(|attempt| attempt.reported).unwrap_or(true));
    gate["execution"] = serde_json::json!(latest
        .map(|attempt| attempt.execution)
        .unwrap_or_else(|| outcome_execution(outcome))
        .as_str());
    if let Some(attempt) = latest {
        gate["result"] = serde_json::json!(attempt_result(attempt));
        gate["exit_code"] = serde_json::json!(attempt.returncode);
        gate["oomed"] = serde_json::json!(attempt.oomed);
        gate["oom_kills"] = serde_json::json!(attempt.oom_kills);
        gate["timed_out"] = serde_json::json!(attempt.timed_out);
        gate["cpu_timed_out"] = serde_json::json!(attempt.cpu_timed_out);
        gate["reason"] = serde_json::json!(attempt.reason);
        gate["aborted"] = serde_json::json!(attempt.aborted);
        gate["real_seconds"] = serde_json::json!(attempt.reported.then_some(attempt.duration_s));
        if let Some(failure_class) = attempt.failure_class {
            gate["failure_class"] = serde_json::json!(failure_class);
        } else {
            gate.as_object_mut()
                .expect("ledger gate must remain a JSON object")
                .remove("failure_class");
        }
        if let Some(failure_detail) = &attempt.failure_detail {
            gate["failure_detail"] = serde_json::json!(failure_detail);
        } else {
            gate.as_object_mut()
                .expect("ledger gate must remain a JSON object")
                .remove("failure_detail");
        }
        set_gate_failure_evidence(&mut gate, attempt_is_failure(attempt));
    }
    // Keep the latest raw observation, including UNKNOWN, alongside the node's
    // aggregate result. A later actual pass recovers a failure; an unknown or
    // infrastructure attempt cannot erase a recorded product failure.
    let classification = node_classification(outcome, attempts);
    gate["raw_result"] = gate["result"].clone();
    gate["raw_aborted"] = gate["aborted"].clone();
    if matches!(classification, NodeClassification::Pass | NodeClassification::ProductFailure) {
        // Parent readers discard aborted gates before consulting failure_class.
        // This is the aggregate result, while raw_aborted and the attempts retain
        // the latest actual cancellation without erasing an earlier failure.
        gate["aborted"] = serde_json::json!(false);
    }
    gate["result"] = serde_json::json!(classification.result());
    // Existing readers use failure_class as the gate's authority. Preserve the
    // observation it replaces explicitly and leave every attempt unchanged.
    for field in ["failure_class", "failure_detail"] {
        if let Some(raw) = gate.get(field).cloned() {
            gate[format!("raw_{field}")] = raw;
        }
        gate.as_object_mut().expect("gate is an object").remove(field);
    }
    if classification != NodeClassification::Pass {
        gate["failure_class"] = serde_json::json!(classification.as_str());
        let cause = node_attempts_raw.iter().rev()
            .find(|attempt| attempt_classification(attempt) == classification);
        if let Some(cause) = cause {
            if classification == NodeClassification::UnderstoodInfrastructureFailure {
                gate["failure_detail"] = serde_json::json!(cause.understood_infrastructure_class);
            } else if classification == NodeClassification::ProductFailure {
                if !cause.reason.is_empty() {
                    gate["failure_detail"] = serde_json::json!(cause.reason);
                }
            } else if let Some(detail) = &cause.failure_detail {
                gate["failure_detail"] = serde_json::json!(detail);
            }
        }
    }
    gate["non_product_failure_bucket"] = serde_json::json!(classification.non_product_failure_bucket());
    set_gate_failure_evidence(&mut gate, classification == NodeClassification::ProductFailure);
    gate["attempts"] = serde_json::json!(node_attempts);
    gate["retries"] = serde_json::json!(node_attempts_raw.len().saturating_sub(1));
    gate["first_attempt_result"] = serde_json::json!(first.and_then(attempt_result));
    gate["first_attempt_reason"] =
        serde_json::json!(first.map(|attempt| attempt.reason.as_str()));
    gate
}

fn typed_gate_round_trip_bracket() -> Result<Vec<serde_json::Value>, String> {
    let fields = ["oomed", "oom_kills", "timed_out", "cpu_timed_out"];
    let mut fixtures = Vec::new();
    for bits in 0_u8..8 {
        let outcome = StepOutcome::failed(
            "test.typed-termination".into(),
            1.0,
            String::new(),
            Some(-9),
            bits & 1 != 0,
            if bits & 1 != 0 { 2 } else { 0 },
            bits & 2 != 0,
            600,
            bits & 4 != 0,
            300,
            300,
            1.0,
            "",
            false,
            None,
            None,
        );
        let first = reported_attempt(&outcome, 1);
        let expected = serde_json::json!({
            "oomed": bits & 1 != 0,
            "oom_kills": if bits & 1 != 0 { 2 } else { 0 },
            "timed_out": bits & 2 != 0,
            "cpu_timed_out": bits & 4 != 0,
        });
        let fallback = ledger_gate(&outcome);
        let reported = ledger_gate_with_attempts(&outcome, std::slice::from_ref(&first));
        for row in [&fallback, &reported, &reported["attempts"][0]] {
            for field in fields {
                if row.get(field) != expected.get(field) {
                    return Err(format!("typed gate {bits}: {field} was lost: {row}"));
                }
            }
        }
        let pass =
            StepOutcome::passed(outcome.tag.clone(), 1.0, String::new(), Some(0), None, None);
        let passed =
            ledger_gate_with_attempts(&outcome, &[first.clone(), reported_attempt(&pass, 2)]);
        let unknown = ledger_gate_with_attempts(
            &outcome,
            &[first, unreported_attempt(outcome.tag.clone(), 2)],
        );
        for field in fields {
            let passed_value = if field == "oom_kills" {
                serde_json::json!(0)
            } else {
                serde_json::json!(false)
            };
            if passed.get(field) != Some(&passed_value)
                || passed["attempts"][1].get(field) != Some(&passed_value)
                || unknown.get(field) != Some(&serde_json::Value::Null)
                || unknown["attempts"][1].get(field) != Some(&serde_json::Value::Null)
                || unknown["attempts"][0].get(field) != expected.get(field)
            {
                return Err(format!(
                    "typed gate {bits}: {field} failed latest-attempt replacement: pass={passed} unknown={unknown}"
                ));
            }
        }
        if passed["result"] != "pass"
            || passed.get("failure_class").is_some()
            || unknown.get("raw_result") != Some(&serde_json::Value::Null)
            || unknown["result"] != "fail"
            || unknown["failure_class"] != "product_failure"
            || unknown["raw_failure_class"] != "no_result"
            || unknown["raw_failure_detail"] != "no completion payload was reported for this node"
        {
            return Err(format!(
                "typed gate {bits}: latest verdict did not follow its attempt"
            ));
        }
        for row in [fallback, reported, passed, unknown] {
            // The parent imports this exact shared type. Additive fields remain
            // in its extra map, including explicit null and the full attempt list.
            let parsed: hermit_manifest_plan::ledger::GateHistoryRow =
                serde_json::from_value(row.clone())
                    .map_err(|error| format!("typed gate reader refused emitted row: {error}"))?;
            let restored = serde_json::to_value(parsed)
                .map_err(|error| format!("typed gate reader could not serialize row: {error}"))?;
            for field in fields.into_iter().chain(["attempts"]) {
                if row.get(field) != restored.get(field) {
                    return Err(format!(
                        "typed gate shared reader lost {field}: before={row} after={restored}"
                    ));
                }
            }
            fixtures.push(row);
        }
    }
    Ok(fixtures)
}

fn ledger_gate_origin_bracket() -> Result<(), String> {
    let failed = StepOutcome {
        tag: "test.fixture".into(),
        ok: false,
        duration_s: 5.0,
        summary: String::new(),
        executed_tests: Some(1),
        filtered_tests: Some(0),
        test_results: None,
        returncode: Some(1),
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: "fixture failure".into(),
        aborted: false,
    };
    let row = ledger_gate(&failed);
    if row["failure_origin"] != "outer_gate"
        || row["failed_substeps"] != serde_json::json!([])
        || row["failure_class"] != "product_failure"
        || row["oomed"] != false
        || row["oom_kills"] != 0
        || row["timed_out"] != false
        || row["cpu_timed_out"] != false
    {
        return Err(
            "ledger gate origin: failed outer gate did not carry its typed product class, termination facts and known-empty substep list".into(),
        );
    }
    let mut passed = failed.clone();
    passed.ok = true;
    passed.returncode = Some(0);
    passed.reason.clear();
    let row = ledger_gate(&passed);
    if !row["failure_origin"].is_null()
        || row.get("failed_substeps").is_some()
        || row.get("failure_class").is_some()
    {
        return Err("ledger gate origin: passing gate claimed failure evidence".into());
    }
    let mut no_result = failed.clone();
    no_result.returncode = Some(NO_RESULT_EXIT_CODE);
    let row = ledger_gate(&no_result);
    if row["result"] != "no_result"
        || row["failure_class"] != "no_result"
        || !row["failure_origin"].is_null()
        || row.get("failed_substeps").is_some()
    {
        return Err("ledger gate origin: no-result gate claimed failure evidence".into());
    }
    let mut aborted = failed.clone();
    aborted.aborted = true;
    aborted.returncode = Some(-15);
    let row = ledger_gate(&aborted);
    if !row["failure_origin"].is_null() || row.get("failed_substeps").is_some() {
        return Err("ledger gate origin: aborted gate claimed failure evidence".into());
    }

    // The stop-path integration test reaches the real write_ledger function but
    // deliberately has no scheduler attempts. Bracket the production serializer's
    // latest-attempt override separately, including stale cumulative outcomes.
    let assert_attempt_gate = |label: &str,
                               outcome: &StepOutcome,
                               attempts: &[NodeAttempt],
                               expected_result: Option<&str>,
                               aggregate_result: &str,
                               expected_reported: bool,
                               expected_execution: &str,
                               expected_failure: bool|
     -> Result<(), String> {
        let row = ledger_gate_with_attempts(outcome, attempts);
        let latest = row["attempts"]
            .as_array()
            .and_then(|attempts| attempts.last())
            .ok_or_else(|| format!("ledger gate origin: {label} omitted attempt history"))?;
        let result = row.get("raw_result").and_then(serde_json::Value::as_str);
        let origin = row
            .get("failure_origin")
            .and_then(serde_json::Value::as_str);
        let failure_evidence_matches = if expected_failure {
            origin == Some("outer_gate")
                && row.get("failed_substeps") == Some(&serde_json::json!([]))
        } else {
            origin.is_none() && row.get("failed_substeps").is_none()
        };
        if result != expected_result
            || row["result"] != aggregate_result
            || row["reported"].as_bool() != Some(expected_reported)
            || row["execution"].as_str() != Some(expected_execution)
            || latest.get("result").and_then(serde_json::Value::as_str) != expected_result
            || latest["reported"].as_bool() != Some(expected_reported)
            || latest["execution"].as_str() != Some(expected_execution)
            || !failure_evidence_matches
        {
            return Err(format!(
                "ledger gate origin: {label} did not follow the latest attempt: {row}"
            ));
        }
        Ok(())
    };

    let failed_attempt = reported_attempt(&failed, 1);
    let passed_attempt = reported_attempt(&passed, 1);
    let aborted_attempt = reported_attempt(&aborted, 1);
    assert_attempt_gate(
        "latest genuine failure",
        &failed,
        std::slice::from_ref(&failed_attempt),
        Some("fail"),
        "fail",
        true,
        "completed",
        true,
    )?;
    assert_attempt_gate(
        "latest pass",
        &passed,
        std::slice::from_ref(&passed_attempt),
        Some("pass"),
        "pass",
        true,
        "completed",
        false,
    )?;
    assert_attempt_gate(
        "latest aborted attempt",
        &aborted,
        std::slice::from_ref(&aborted_attempt),
        None,
        "no_result",
        true,
        "unknown",
        false,
    )?;

    let mut passed_retry = passed_attempt.clone();
    passed_retry.attempt = 2;
    let fail_then_pass = [failed_attempt.clone(), passed_retry];
    assert_attempt_gate(
        "stale failure followed by pass",
        &failed,
        &fail_then_pass,
        Some("pass"),
        "pass",
        true,
        "completed",
        false,
    )?;
    let mut aborted_retry = aborted_attempt.clone();
    aborted_retry.attempt = 2;
    let fail_then_aborted = [failed_attempt.clone(), aborted_retry];
    assert_attempt_gate(
        "stale failure followed by abort",
        &failed,
        &fail_then_aborted,
        None,
        "fail",
        true,
        "unknown",
        true,
    )?;
    let unreported_retry = unreported_attempt(failed.tag.clone(), 2);
    let fail_then_unreported = [failed_attempt.clone(), unreported_retry];
    let unreported_gate = ledger_gate_with_attempts(&failed, &fail_then_unreported);
    for row in [&unreported_gate, &unreported_gate["attempts"][1]] {
        for field in ["oomed", "oom_kills", "timed_out", "cpu_timed_out"] {
            if row.get(field) != Some(&serde_json::Value::Null) {
                return Err(format!(
                    "ledger gate origin: latest unreported {field} must be present and null: {row}"
                ));
            }
        }
    }
    if !unreported_gate["oomed"].is_null()
        || !unreported_gate["oom_kills"].is_null()
        || !unreported_gate["timed_out"].is_null()
        || !unreported_gate["cpu_timed_out"].is_null()
        || !unreported_gate["attempts"][1]["oomed"].is_null()
        || !unreported_gate["attempts"][1]["oom_kills"].is_null()
        || !unreported_gate["attempts"][1]["timed_out"].is_null()
        || !unreported_gate["attempts"][1]["cpu_timed_out"].is_null()
    {
        return Err(format!(
            "ledger gate origin: an unreported attempt fabricated typed termination facts: {unreported_gate}"
        ));
    }
    let mut failed_retry = failed_attempt;
    failed_retry.attempt = 2;
    let pass_then_failure = [passed_attempt, failed_retry];
    assert_attempt_gate(
        "stale pass followed by genuine failure",
        &passed,
        &pass_then_failure,
        Some("fail"),
        "fail",
        true,
        "completed",
        true,
    )?;

    for (label, detail, want_class, want_detail) in [
        (
            "infrastructure",
            "error: failed to verify the checksum for `fixture v1.0.0`",
            FailureClass::UnderstoodInfrastructureFailure,
            "failed to verify the checksum",
        ),
        (
            "prerequisite",
            "prepare failed for language-runtimes/lua-random.sh\nno Lua interpreter on PATH",
            FailureClass::UnderstoodPrerequisiteFailure,
            " on path",
        ),
    ] {
        let mut classified = reported_attempt(&failed, 1);
        let environmental = validate_runtime::environmental_block_class(detail);
        let failure = validate_runtime::failure_class_from_detail(detail);
        stamp_attempt_detail(
            std::slice::from_mut(&mut classified),
            &failed.tag,
            environmental,
            validate_runtime::understood_infrastructure_class(detail),
            failure,
        );
        let row = ledger_gate_with_attempts(&failed, std::slice::from_ref(&classified));
        if row["raw_failure_class"] != want_class.as_str()
            || row["raw_failure_detail"] != want_detail
            || row["attempts"][0]["failure_class"] != want_class.as_str()
            || row["attempts"][0]["failure_detail"] != want_detail
        {
            return Err(format!(
                "ledger gate failure class: {label} was not written as a closed class plus detail: {row}"
            ));
        }
    }
    typed_gate_round_trip_bracket()?;
    println!("  ledger gate origin: fallback, terminal attempts, and failure classes stayed typed");
    Ok(())
}

fn requalification_plan_bracket(root: &Path) -> Result<(), String> {
    let source_path = validate_plan::validation_dag_path(root);
    let source_before = std::fs::read(&source_path)
        .map_err(|error| format!("requalification plan: cannot read source DAG: {error}"))?;
    let args = parse_argv(&[
        "--requalify-cell".into(),
        "applications/timed-progress-bar".into(),
        "verify".into(),
        "ptrace".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("requalification plan: CLI refused with exit {code}"))?;
    let mut plan = build_plan(root, &args, &std::env::temp_dir())?;
    if plan.suite_complete
        || plan.selection_mode != "targeted"
        || plan.second.is_some()
        || plan.cell_evidence_expected.as_ref().is_none_or(Vec::is_empty)
    {
        return Err("requalification plan: owning-step selection gained full-suite authority".into());
    }

    let target = validate_cell_results::expected_plan(root)?
        .into_iter()
        .find(|cell| {
            cell["test"] == "applications/timed-progress-bar"
                && cell["mode"] == "verify"
                && cell["backend"] == "ptrace"
        })
        .ok_or("requalification plan: exact target disappeared from expected plan")?;
    let exact = DagManifest {
        lane: requalification_identity_field(&target, "lane")?.into(),
        category: requalification_identity_field(&target, "category")?.into(),
        test: Some(requalification_identity_field(&target, "test")?.into()),
        mode: Some(requalification_identity_field(&target, "mode")?.into()),
        backend: Some(requalification_identity_field(&target, "backend")?.into()),
    };
    let committed = validate_plan::validation_config(root)?;
    let lane = dagrun::select_steps_by_labels(
        &committed,
        std::slice::from_ref(&exact.lane),
    )?;
    let owner = dagrun::result_manifest_owner(&lane.steps, &exact)?;
    let owner_tag = owner.tag();
    let expected_cfg =
        dagrun::select_steps_by_tags(&lane, std::slice::from_ref(&owner_tag), false)?;
    if dag_to_json(&plan.cfg) != dag_to_json(&expected_cfg) {
        return Err(format!(
            "requalification plan: {owner_tag} and its committed dependency closure changed before scheduling"
        ));
    }
    let selected_owner = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == owner_tag)
        .ok_or_else(|| format!("requalification plan: selected owner {owner_tag} disappeared"))?;
    if selected_owner.effective_result_manifests() != owner.effective_result_manifests() {
        return Err(format!(
            "requalification plan: selected owner {owner_tag} changed its declared cell outcomes"
        ));
    }
    let declared_population = owner
        .effective_result_manifests()
        .iter()
        .map(exact_manifest_value)
        .collect::<Result<Vec<_>, _>>()?;
    if plan.cell_evidence_expected.as_ref() != Some(&declared_population)
        || !declared_population.contains(&target)
    {
        return Err(format!(
            "requalification plan: retained population does not equal {owner_tag}'s declared results or omits the requested cell"
        ));
    }
    if plan.cfg.steps.iter().any(|step| {
        step.group == "cell"
            || step.cmd.contains("pressure-test.rs")
            || step.cmd.contains("run_dag_boxed_deadline")
            || step.cmd.contains("dagrun run")
    }) {
        return Err("requalification plan: exact-result selection synthesized a node or nested scheduler".into());
    }
    require_committed_scheduler_input(&plan)?;

    let selected_owner = plan
        .cfg
        .steps
        .iter_mut()
        .find(|step| step.tag() == owner_tag)
        .expect("owner checked above");
    let original_results = selected_owner.result_manifests.clone();
    selected_owner.result_manifests = Some(Vec::new());
    let mutation_error = require_committed_scheduler_input(&plan)
        .err()
        .ok_or("requalification plan: planted result-ownership mutation reached the scheduler")?;
    if !mutation_error.contains("changed after selection") {
        return Err(format!(
            "requalification plan: ownership-mutation refusal was not specific: {mutation_error}"
        ));
    }
    plan.cfg
        .steps
        .iter_mut()
        .find(|step| step.tag() == owner_tag)
        .expect("owner checked above")
        .result_manifests = original_results;
    require_committed_scheduler_input(&plan)?;

    let moving_source = tempfile::Builder::new()
        .prefix("validate-moving-dag-")
        .tempdir()
        .map_err(|error| format!("requalification plan: cannot create source fixture: {error}"))?;
    let moving_path = moving_source.path().join("validate.json");
    std::fs::write(&moving_path, &source_before)
        .map_err(|error| format!("requalification plan: cannot seed source fixture: {error}"))?;
    plan.committed_source = Some((moving_path.clone(), source_before.clone()));
    let mut changed = source_before.clone();
    changed.push(b'\n');
    std::fs::write(&moving_path, changed)
        .map_err(|error| format!("requalification plan: cannot mutate source fixture: {error}"))?;
    let source_error = require_committed_scheduler_input(&plan)
        .err()
        .ok_or("requalification plan: moving committed source reached the scheduler")?;
    if !source_error.contains("changed after selection") {
        return Err(format!(
            "requalification plan: moving-source refusal was not specific: {source_error}"
        ));
    }

    let missing_args = parse_argv(&[
        "--requalify-cell".into(),
        "applications/no-such-test".into(),
        "verify".into(),
        "ptrace".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("requalification plan: missing-cell CLI failed with exit {code}"))?;
    let missing = build_plan(root, &missing_args, &std::env::temp_dir())
        .err()
        .ok_or("requalification plan: unknown exact cell was accepted")?;
    if !missing.contains("exactly one currently selected cell") {
        return Err(format!(
            "requalification plan: missing-cell refusal was not specific: {missing}"
        ));
    }

    let source_after = std::fs::read(&source_path)
        .map_err(|error| format!("requalification plan: cannot re-read source DAG: {error}"))?;
    if source_after != source_before {
        return Err("requalification plan: exact-result selection changed ci/dag/validate.json".into());
    }
    println!(
        "  requalification plan: exact five-field identity resolved to committed owner {owner_tag}; its dependency closure and declared result population stayed unchanged; graph and source mutations both refused"
    );
    Ok(())
}

fn tool_root_split_bracket() -> Result<(), String> {
    let root = std::env::temp_dir().join(format!(
        "validate-tool-root-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    let state_root = root.join("state-root");
    let tool_root = root.join("tool-root");
    let frozen_root = root.join("frozen-root");
    let target_root = root.join(
        "cache/trees/1111111111111111111111111111111111111111",
    );
    let other_root = root.join("other-root");
    let fake_root = root.join("fake-root");
    let checkout = root.join("checkout");
    let log = root.join("validate.log");
    let git = |dir: &Path, args: &[&str]| -> Result<(), String> {
        let status = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(["-C", dir.to_str().ok_or("tool-root split: non-UTF-8 path")?])
            .args(args)
            .status()
            .map_err(|error| format!("tool-root split: cannot run git: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("tool-root split: git {args:?} exited {status}"))
        }
    };
    std::fs::create_dir_all(state_root.join("ci-hub/validate"))
        .and_then(|_| std::fs::create_dir_all(state_root.join("ci-hub/ledger")))
        .and_then(|_| std::fs::create_dir_all(&checkout))
        .and_then(|_| std::fs::create_dir_all(fake_root.join("ci-hub")))
        // A real validate points TMPDIR inside its worktree. Without this
        // invalid marker, Git walks upward from fake_root, finds that outer
        // checkout, and this no longer exercises the non-repository refusal.
        .and_then(|_| std::fs::write(fake_root.join(".git"), "not a git worktree\n"))
        .map_err(|error| format!("tool-root split: cannot create fixture: {error}"))?;
    std::fs::write(
        state_root.join(".gitmodules"),
        "[submodule \"hermit\"]\n\tpath = hermit\n\turl = fixture://hermit\n",
    )
    .and_then(|_| {
        std::fs::write(
            state_root.join("ci-hub/ci-hub"),
            "#!/bin/sh\n: > \"$DEV_HERMIT_PARENT/authority-called\"\nexit 23\n",
        )
    })
    .and_then(|_| {
        std::fs::write(
            state_root.join("ci-hub/validate/finalize_receipt.py"),
            "import json\nfrom pathlib import Path\nroot = Path(__file__).resolve().parents[2]\nprint(json.dumps({'base_sha':root.name,'base_tree':'tool-tree','reverie_base_sha':'rev','reverie_base_tree':'rev-tree'}))\n",
        )
    })
    .and_then(|_| {
        std::fs::write(
            state_root.join("ci-hub/ledger/validate_rows.py"),
            "import json\nfrom pathlib import Path\nprint(json.dumps({'adapter_root': Path(__file__).resolve().parents[2].name}))\n",
        )
    })
    .and_then(|_| std::fs::write(&log, "fixture\n"))
    .map_err(|error| format!("tool-root split: cannot write fixture: {error}"))?;
    std::fs::set_permissions(
        state_root.join("ci-hub/ci-hub"),
        std::fs::Permissions::from_mode(0o755),
    )
    .map_err(|error| format!("tool-root split: cannot chmod fixture: {error}"))?;
    git(&state_root, &["init", "-b", "main"])?;
    git(&state_root, &["config", "user.email", "fixture@example.com"])?;
    git(&state_root, &["config", "user.name", "fixture"])?;
    git(&state_root, &["add", "."])?;
    git(&state_root, &["commit", "-m", "fixture"])?;
    git(
        &state_root,
        &["worktree", "add", "-b", "tool", tool_root.to_str().unwrap()],
    )?;
    git(
        &root,
        &["clone", state_root.to_str().unwrap(), other_root.to_str().unwrap()],
    )?;

    std::fs::create_dir_all(frozen_root.join("ci-hub/validate"))
        .and_then(|_| std::fs::create_dir_all(frozen_root.join("ci-hub/ledger")))
        .and_then(|_| {
            std::fs::copy(
                state_root.join("ci-hub/ci-hub"),
                frozen_root.join("ci-hub/ci-hub"),
            )
        })
        .and_then(|_| {
            std::fs::copy(
                state_root.join("ci-hub/validate/finalize_receipt.py"),
                frozen_root.join("ci-hub/validate/finalize_receipt.py"),
            )
        })
        .and_then(|_| {
            std::fs::copy(
                state_root.join("ci-hub/ledger/validate_rows.py"),
                frozen_root.join("ci-hub/ledger/validate_rows.py"),
            )
        })
        .map_err(|error| format!("tool-root split: cannot create frozen fixture: {error}"))?;
    std::fs::set_permissions(
        frozen_root.join("ci-hub/ci-hub"),
        std::fs::Permissions::from_mode(0o755),
    )
    .map_err(|error| format!("tool-root split: cannot chmod frozen fixture: {error}"))?;
    make_self_test_tree_read_only(&frozen_root)?;
    std::fs::create_dir_all(&target_root)
        .map_err(|error| format!("tool-root split: cannot create cached target: {error}"))?;
    std::fs::set_permissions(&target_root, std::fs::Permissions::from_mode(0o555))
        .map_err(|error| format!("tool-root split: cannot freeze cached target: {error}"))?;

    let saved_tool_root = std::env::var_os(TOOL_ROOT_ENV);
    let saved_ledger = std::env::var_os(LEDGER_ENV);
    let saved_parent = std::env::var_os(PARENT_ENV);
    let authority_environment = [
        TOOL_AUTHORITY_ENV,
        TOOL_CONTENT_SHA256_ENV,
        TOOL_PARENT_SHA_ENV,
        TOOL_HERMIT_SHA_ENV,
        TOOL_AGENT_UTILS_SHA_ENV,
        TOOL_BOOTSTRAP_SHA256_ENV,
    ];
    let saved_authority_environment: Vec<(&str, Option<std::ffi::OsString>)> = authority_environment
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect();
    for name in authority_environment {
        // SAFETY: validate's self-test is single-threaded and restores these values.
        unsafe { std::env::remove_var(name) };
    }
    // SAFETY: validate's self-test is single-threaded and restores this value.
    unsafe { std::env::set_var(TOOL_ROOT_ENV, "relative/tool-root") };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("must be absolute"))
    {
        return Err("tool-root split: relative explicit tool root did not refuse".into());
    }
    unsafe { std::env::set_var(TOOL_ROOT_ENV, root.join("missing")) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("cannot resolve explicit"))
    {
        return Err("tool-root split: missing explicit tool root did not refuse".into());
    }
    unsafe { std::env::set_var(TOOL_ROOT_ENV, &fake_root) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("not a readable Git worktree"))
    {
        return Err("tool-root split: non-repository tool root did not refuse".into());
    }
    unsafe { std::env::set_var(TOOL_ROOT_ENV, &other_root) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("is not a worktree of canonical"))
    {
        return Err("tool-root split: unrelated repository tool root did not refuse".into());
    }
    std::fs::write(tool_root.join("dirty"), "fixture\n")
        .map_err(|error| format!("tool-root split: cannot dirty fixture: {error}"))?;
    unsafe { std::env::set_var(TOOL_ROOT_ENV, &tool_root) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains(" is dirty:"))
    {
        return Err("tool-root split: dirty explicit tool root did not refuse".into());
    }
    std::fs::remove_file(tool_root.join("dirty"))
        .map_err(|error| format!("tool-root split: cannot clean fixture: {error}"))?;
    let resolved = configured_tool_root(Some(&state_root))?
        .ok_or("tool-root split: explicit tool root disappeared")?;
    if resolved != std::fs::canonicalize(&tool_root).map_err(|error| error.to_string())? {
        return Err("tool-root split: explicit tool root resolved to another checkout".into());
    }

    let valid_authority = create_self_test_tool_authority(
        &frozen_root,
        &state_root,
        &target_root,
        None,
        false,
        false,
        None,
    )?;
    install_self_test_tool_authority(&valid_authority);
    let retained_root = configured_tool_root(Some(&state_root))?
        .ok_or("tool-root split: producer-authorized root disappeared")?;
    if retained_root != valid_authority.tool_root {
        return Err(
            "tool-root split: producer-authorized root was detached from its retained descriptor"
                .into(),
        );
    }

    // An ordinary directory with identical bytes is not the descriptor-backed
    // capability the producer authorized.
    unsafe { std::env::set_var(TOOL_ROOT_ENV, &frozen_root) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("must be an exact /proc/<pid>/fd/<fd> capability"))
    {
        return Err("tool-root split: pathname-only immutable root did not refuse".into());
    }
    install_self_test_tool_authority(&valid_authority);

    unsafe { std::env::remove_var(TOOL_CONTENT_SHA256_ENV) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("authority is incomplete"))
    {
        return Err("tool-root split: incomplete immutable authority did not refuse".into());
    }
    install_self_test_tool_authority(&valid_authority);

    unsafe {
        std::env::set_var(
            TOOL_PARENT_SHA_ENV,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
    };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("does not match immutable tool authority"))
    {
        return Err("tool-root split: wrong authority parent SHA did not refuse".into());
    }
    install_self_test_tool_authority(&valid_authority);

    let copied_authority = root.join("copied-authority.json");
    std::fs::copy(&valid_authority.authority_path, &copied_authority)
        .and_then(|_| {
            std::fs::set_permissions(
                &copied_authority,
                std::fs::Permissions::from_mode(0o444),
            )
        })
        .map_err(|error| format!("tool-root split: cannot copy authority fixture: {error}"))?;
    unsafe { std::env::set_var(TOOL_AUTHORITY_ENV, &copied_authority) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("must be an exact /proc/<pid>/fd/<fd> capability"))
    {
        return Err("tool-root split: copied authority record did not refuse".into());
    }
    install_self_test_tool_authority(&valid_authority);

    let other_fd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&other_root)
        .map_err(|error| format!("tool-root split: cannot retain wrong root: {error}"))?;
    let wrong_target_authority = create_self_test_tool_authority(
        &frozen_root,
        &state_root,
        &target_root,
        None,
        false,
        false,
        Some(&other_fd),
    )?;
    install_self_test_tool_authority(&wrong_target_authority);
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("target descriptor identity does not match"))
    {
        return Err("tool-root split: wrong target descriptor did not refuse".into());
    }
    install_self_test_tool_authority(&valid_authority);

    let wrong_root = PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        other_fd.as_raw_fd()
    ));
    unsafe { std::env::set_var(TOOL_ROOT_ENV, &wrong_root) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("does not name the supplied descriptors"))
    {
        return Err("tool-root split: replaced root descriptor did not refuse".into());
    }
    install_self_test_tool_authority(&valid_authority);

    let wrong_inode_authority = create_self_test_tool_authority(
        &frozen_root,
        &state_root,
        &target_root,
        None,
        true,
        false,
        None,
    )?;
    install_self_test_tool_authority(&wrong_inode_authority);
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("root descriptor identity does not match"))
    {
        return Err("tool-root split: wrong root inode authority did not refuse".into());
    }

    let wrong_target_inode_authority = create_self_test_tool_authority(
        &frozen_root,
        &state_root,
        &target_root,
        None,
        false,
        true,
        None,
    )?;
    install_self_test_tool_authority(&wrong_target_inode_authority);
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("target descriptor identity does not match"))
    {
        return Err("tool-root split: wrong target inode authority did not refuse".into());
    }

    let wrong_digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let wrong_digest_authority = create_self_test_tool_authority(
        &frozen_root,
        &state_root,
        &target_root,
        Some(wrong_digest),
        false,
        false,
        None,
    )?;
    install_self_test_tool_authority(&wrong_digest_authority);
    unsafe { std::env::set_var(TOOL_CONTENT_SHA256_ENV, wrong_digest) };
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("immutable tool content digest is"))
    {
        return Err("tool-root split: wrong content digest authority did not refuse".into());
    }

    let replaceable_state = root.join("replaceable-state");
    let retained_state = root.join("retained-state");
    std::fs::create_dir(&replaceable_state)
        .map_err(|error| format!("tool-root split: cannot create replaceable state: {error}"))?;
    let state_authority = create_self_test_tool_authority(
        &frozen_root,
        &replaceable_state,
        &target_root,
        None,
        false,
        false,
        None,
    )?;
    install_self_test_tool_authority(&state_authority);
    std::fs::rename(&replaceable_state, &retained_state)
        .and_then(|_| std::fs::create_dir(&replaceable_state))
        .map_err(|error| format!("tool-root split: cannot replace state pathname: {error}"))?;
    if !configured_tool_root(Some(&replaceable_state))
        .is_err_and(|error| error.contains("state-root pathname no longer names"))
    {
        return Err("tool-root split: replaced state-root pathname did not refuse".into());
    }
    std::fs::remove_dir(&replaceable_state)
        .and_then(|_| std::fs::rename(&retained_state, &replaceable_state))
        .map_err(|error| format!("tool-root split: cannot restore state pathname: {error}"))?;

    let retained_target = root.join("cache/trees/retained-target");
    std::fs::rename(&target_root, &retained_target)
        .and_then(|_| std::fs::create_dir(&target_root))
        .and_then(|_| {
            std::fs::set_permissions(&target_root, std::fs::Permissions::from_mode(0o555))
        })
        .map_err(|error| format!("tool-root split: cannot replace target pathname: {error}"))?;
    install_self_test_tool_authority(&valid_authority);
    if !configured_tool_root(Some(&state_root))
        .is_err_and(|error| error.contains("target pathname no longer names"))
    {
        return Err("tool-root split: replaced target pathname did not refuse".into());
    }
    std::fs::remove_dir(&target_root)
        .and_then(|_| std::fs::rename(&retained_target, &target_root))
        .map_err(|error| format!("tool-root split: cannot restore target pathname: {error}"))?;

    install_self_test_tool_authority(&valid_authority);
    let effective_tool_root = &valid_authority.tool_root;
    unsafe { std::env::set_var(PARENT_ENV, &state_root) };
    if validate_history::canonical_ledger_adapter(
        &state_root.join("ledger"),
        Some(effective_tool_root),
    ) != Some(effective_tool_root.join("ci-hub/ledger/validate_rows.py"))
    {
        return Err("tool-root split: ledger adapter resolved through the state root".into());
    }
    let tool_authority_marker = state_root.join("authority-called");
    let authority =
        validate_lock_admission(Some(effective_tool_root), "fixture", "fixture-host");
    if authority.is_ok() || !tool_authority_marker.is_file() {
        return Err(
            "tool-root split: authority did not execute exclusively from the tool root".into(),
        );
    }

    let receipt = receipt_evidence(Some(effective_tool_root), &checkout, &log, "fixture");
    if receipt.base_sha != serde_json::json!("frozen-root")
        || receipt.base_tree != serde_json::json!("tool-tree")
    {
        return Err("tool-root split: receipt finalizer did not execute from the tool root".into());
    }

    let refusal = product_front_door_refusal(
        effective_tool_root,
        &checkout,
        "fixture",
        "full --no-label-pr",
        true,
        false,
    )
    .ok_or("tool-root split: product front door omitted refusal")?;
    let tool_launcher = effective_tool_root
        .join("ci-hub/ci-hub")
        .to_string_lossy()
        .into_owned();
    let state_launcher = state_root.join("ci-hub/ci-hub").to_string_lossy().into_owned();
    if !refusal.contains(&tool_launcher) || refusal.contains(&state_launcher) {
        return Err("tool-root split: remediation named the state root instead of tool root".into());
    }

    // The reader and writer share this single adapter-path resolver. Exercise
    // the real reader from the tool checkout while the ledger itself remains
    // rooted under canonical state.
    unsafe { std::env::remove_var(LEDGER_ENV) };
    let rows = validate_history::read_rows(&state_root.join("ledger"));
    if rows.len() != 1 || rows[0]["adapter_root"] != "frozen-root" {
        return Err("tool-root split: canonical ledger adapter executed from state root".into());
    }

    match saved_tool_root {
        Some(value) => unsafe { std::env::set_var(TOOL_ROOT_ENV, value) },
        None => unsafe { std::env::remove_var(TOOL_ROOT_ENV) },
    }
    match saved_ledger {
        Some(value) => unsafe { std::env::set_var(LEDGER_ENV, value) },
        None => unsafe { std::env::remove_var(LEDGER_ENV) },
    }
    match saved_parent {
        Some(value) => unsafe { std::env::set_var(PARENT_ENV, value) },
        None => unsafe { std::env::remove_var(PARENT_ENV) },
    }
    for (name, value) in saved_authority_environment {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(&root).status();
    let _ = std::fs::remove_dir_all(&root);
    println!(
        "  tool-root split: sealed descriptor authority, digest, receipt finalizer, and remediation preserve immutable code/state separation"
    );
    Ok(())
}

fn validate_series_writer_bracket() -> Result<(), String> {
    let root = std::env::temp_dir().join(format!(
        "validate-series-writer-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    let parent = root.join("parent");
    let tool_root = root.join("tool-root");
    let checkout = root.join("checkout");
    let results = root.join("results/bucket");
    std::fs::create_dir_all(&parent)
        .and_then(|_| std::fs::create_dir_all(tool_root.join("ci-hub/series")))
        .and_then(|_| std::fs::create_dir_all(&checkout))
        .and_then(|_| std::fs::create_dir_all(&results))
        .map_err(|error| format!("validate series writer: cannot create fixture: {error}"))?;
    std::fs::write(
        tool_root.join("ci-hub/series/series.py"),
        r#"import json
import pathlib
import sys
parent = pathlib.Path(sys.argv[sys.argv.index("--parent") + 1])
parent.joinpath("captured.json").write_text(json.dumps({"argv": sys.argv[1:], "stdin": sys.stdin.read()}))
print("fixture append accepted")
"#,
    )
    .map_err(|error| format!("validate series writer: cannot write fixture script: {error}"))?;
    let row = |attempt| {
        serde_json::json!({
            "schema": 4,
            "attempt": attempt,
            "hermit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_tree_dirty": false,
            "test": "applications/fixture",
            "category": "applications",
            "lane": "portable",
            "mode": "verify",
            "backend": "ptrace",
            "outcome": if attempt == 1 { "FAIL" } else { "PASS" },
        })
    };
    std::fs::write(
        results.join("results.jsonl"),
        format!("{}\n{}\n", row(1), row(2)),
    )
    .map_err(|error| format!("validate series writer: cannot write result fixture: {error}"))?;
    let saved = std::env::var_os("E2E_RUN_ID");
    // SAFETY: the validate self-test is single-threaded here and restores the
    // process environment before returning.
    unsafe { std::env::set_var("E2E_RUN_ID", "validate-series-fixture") };
    let appended = append_validate_series(
        Some(&parent),
        Some(&tool_root),
        &checkout,
        &root.join("results"),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    match saved {
        Some(value) => unsafe { std::env::set_var("E2E_RUN_ID", value) },
        None => unsafe { std::env::remove_var("E2E_RUN_ID") },
    }
    appended?;
    let captured: serde_json::Value = serde_json::from_slice(
        &std::fs::read(parent.join("captured.json"))
            .map_err(|error| format!("validate series writer: cannot read captured call: {error}"))?,
    )
    .map_err(|error| format!("validate series writer: malformed captured call: {error}"))?;
    let argv = captured["argv"]
        .as_array()
        .ok_or("validate series writer: captured argv is not an array")?;
    let arguments = argv.iter().filter_map(serde_json::Value::as_str).collect::<Vec<_>>();
    for pair in [
        ["--checkout", checkout.to_str().ok_or("fixture checkout is not UTF-8")?],
        ["--producer", "validate"],
        ["--run-id", "validate-series-fixture"],
        ["--tree", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
    ] {
        if !arguments.windows(2).any(|window| window == pair) {
            return Err(format!("validate series writer: omitted argument pair {pair:?}"));
        }
    }
    let rows = captured["stdin"]
        .as_str()
        .ok_or("validate series writer: captured stdin is not a string")?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("validate series writer: malformed captured row: {error}"))?;
    if rows.len() != 2 || rows[0]["attempt"] != 1 || rows[1]["attempt"] != 2 {
        return Err(format!(
            "validate series writer: ordinary appended attempts were not both sent: {rows:?}"
        ));
    }
    let _ = std::fs::remove_dir_all(&root);
    println!("  validate series writer: checkout identity and both appended attempts carried");
    Ok(())
}

fn possible_missing_artifact_bracket() -> Result<(), String> {
    let row = |tag: &str, ok: bool, returncode: Option<i64>, duration_s: f64| StepOutcome {
        tag: tag.into(),
        ok,
        duration_s,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        test_results: None,
        returncode,
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: String::new(),
        aborted: false,
    };
    let outcomes = vec![
        row("fixture.missing", false, Some(127), 1.0),
        row("fixture.slow_127", false, Some(127), 5.0),
        row("fixture.other_exit", false, Some(126), 1.0),
        row("fixture.passed", true, Some(0), 1.0),
    ];
    if possible_missing_artifact_nodes("only", &outcomes) != vec!["fixture.missing"] {
        return Err("missing-artifact hint did not select exactly the fast --only exit-127 row".into());
    }
    for mode in ["full", "targeted", "selective"] {
        if !possible_missing_artifact_nodes(mode, &outcomes).is_empty() {
            return Err(format!(
                "missing-artifact hint escaped --only into selection mode {mode}"
            ));
        }
    }
    println!(
        "  missing-artifact hint: 1 --only candidate / 3 non-qualifying shapes; other profiles silent"
    );
    Ok(())
}

fn no_result_propagation_bracket() -> Result<(), String> {
    let outcome = |tag: &str, returncode: i64, aborted: bool| StepOutcome {
        tag: tag.into(),
        ok: returncode == 0 && !aborted,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        test_results: None,
        returncode: Some(returncode),
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: String::new(),
        aborted,
    };

    let pass = outcome("pass", 0, false);
    if outcome_is_no_result(&pass)
        || outcome_is_failure(&pass)
        || ledger_gate_result(&pass) != "pass"
        || completed_exit_code(0, 0, false, false) != 0
        || ledger_run_results(0, 0, 0, false) != ("pass", "pass")
    {
        return Err("no-result propagation: exit 0 no longer stays PASS".into());
    }

    let no_result = outcome("no-result", NO_RESULT_EXIT_CODE, false);
    if !outcome_is_no_result(&no_result)
        || outcome_is_failure(&no_result)
        || ledger_gate_result(&no_result) != "no_result"
        || completed_exit_code(0, 1, false, false) != NO_RESULT_EXIT_CODE as u8
        || ledger_run_results(NO_RESULT_EXIT_CODE as u8, 0, 1, false)
            != ("fail", "no_result")
        || ledger_run_results(NO_RESULT_EXIT_CODE as u8, 0, 0, false)
            != ("fail", "no_result")
    {
        return Err("no-result propagation: exit 75 did not remain a distinct NO_RESULT".into());
    }
    let no_result_attempt = reported_attempt(&no_result, 1);
    let no_result_gate = ledger_gate_with_attempts(&no_result, std::slice::from_ref(&no_result_attempt));
    if no_result_gate["result"] != "no_result"
        || no_result_gate["attempts"][0]["result"] != "no_result"
        || no_result_gate["failure_class"] != "no_result"
        || no_result_gate["attempts"][0]["failure_class"] != "no_result"
        || !no_result_gate["failure_origin"].is_null()
        || no_result_gate.get("failed_substeps").is_some()
    {
        return Err(format!(
            "no-result propagation: exact-attempt ledger weakened exit 75: {no_result_gate}"
        ));
    }

    // A completed no-result retry ran, but it did not produce a pass/fail
    // verdict. It therefore cannot confirm or refute an earlier environmental
    // hypothesis; the hypothesis remains explicitly UNCONFIRMED.
    let initial_environmental = outcome("environmental-no-result", 1, false);
    let retry_no_result = outcome("environmental-no-result", NO_RESULT_EXIT_CODE, false);
    let mut initial_attempt = reported_attempt(&initial_environmental, 1);
    initial_attempt.environmental_class = Some("bpfjailer-banner".into());
    initial_attempt.detail_observed = true;
    let no_result_retry_attempt = reported_attempt(&retry_no_result, 2);
    let no_result_attempts = [initial_attempt, no_result_retry_attempt];
    if environmental_assessment(&no_result_attempts, &no_result_attempts[0])
        != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
    {
        return Err(
            "no-result propagation: exit 75 falsely settled an environmental hypothesis".into(),
        );
    }

    for returncode in [-9, 1, 2, 3, 74, 76, 124, 127] {
        let failure = outcome("failure", returncode, false);
        if outcome_is_no_result(&failure)
            || !outcome_is_failure(&failure)
            || ledger_gate_result(&failure) != "fail"
            || completed_exit_code(1, 0, false, false) != 1
            || ledger_run_results(1, 1, 0, false) != ("fail", "fail")
        {
            return Err(format!(
                "no-result propagation: genuine failure exit {returncode} was weakened"
            ));
        }
    }

    if completed_exit_code(1, 1, false, false) != 1
        || ledger_run_results(1, 1, 1, false) != ("fail", "fail")
        || ledger_run_results(1, 0, 1, false) != ("fail", "fail")
        || ledger_run_results(NO_RESULT_EXIT_CODE as u8, 1, 1, false) != ("fail", "fail")
        || ledger_run_results(NO_RESULT_EXIT_CODE as u8, 1, 0, false) != ("fail", "fail")
    {
        return Err("no-result propagation: a sibling exit 75 hid a genuine failure".into());
    }

    // A whole-run cutoff leaves selected work unmeasured. It never erases a
    // completed product failure, including an individually collected node limit.
    if completed_exit_code(0, 1, true, false) != NO_RESULT_EXIT_CODE as u8
        || completed_exit_code(1, 1, true, false) != 1
    {
        return Err("no-result propagation: run cutoff lost incomplete/product-failure distinction".into());
    }
    if completed_exit_code(0, 1, false, true) != 1 {
        return Err(
            "no-result propagation: an unexplained runner failure was weakened to NO_RESULT".into(),
        );
    }

    let aborted = outcome("aborted", NO_RESULT_EXIT_CODE, true);
    if outcome_is_no_result(&aborted) || outcome_is_failure(&aborted) {
        return Err("no-result propagation: an aborted row acquired a completed verdict".into());
    }

    println!(
        "  no-result propagation: 75 stayed distinct; 0 passed; 8 other exits and mixed 75+failure stayed RED"
    );
    Ok(())
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
    // Every attempt of every node, so a retried node's superseded verdict
    // reaches the row instead of being replaced by the one that followed it.
    attempts: &[NodeAttempt],
    skipped: &[String],
    host_inapplicable: &[validate_plan::HostInapplicableNode],
    planned_tags: &BTreeSet<String>,
    wall_s: f64,
    exit_code: u8,
    log_file: &str,
    execution_complete: bool,
    coverage: serde_json::Value,
    cell_results: Option<&validate_cell_results::RetainedCellResults>,
) {
    let (coverage_schema, coverage) = ledger_schema_and_coverage(coverage);
    let ledger_schema = ledger_schema_version(coverage_schema, cell_results);
    // `gate_records` counts typed scheduler outcomes, including an explicit
    // UNKNOWN record for a spawn/supervisor failure. `executed_nodes` counts
    // only terminal attempts with a collected child exit status. The two must
    // not share one integer: retaining an unknown row is required, but calling
    // it executed would turn missing evidence into a measurement.
    let gate_records = outcomes.len();
    let executed_nodes = u64::try_from(completed_node_count(outcomes, attempts))
        .expect("executed node count fits u64");
    let classification = classify_run(outcomes, attempts, skipped, planned_tags, host_inapplicable);
    let failures = classification.product_failure_nodes.len();
    let no_results = classification.no_results();
    let validation_complete = validation_is_complete(execution_complete, &classification, planned_tags);
    // An operator stop learned nothing new about the product. Preserve the raw
    // shell outcome for forensics, but do not mint a FAILED verdict unless a
    // completed gate had already established one before the stop
    // (validate.sh:1473 `interruption_is_no_result`).
    let (raw_result, result) =
        ledger_run_results(exit_code, failures, no_results, ctx.interruption.is_some());
    let timed_out = timed_out_nodes(outcomes);
    // Stable per-row identity. Corrections never edit a row; they append a new
    // one carrying `corrects: <this id>`, which is what keeps the shard
    // append-only and safe to union across machines.
    let record_id = format!("{}-{}-{}", ctx.host, epoch_now(), std::process::id());
    // Retain the exact selected denominator on incomplete and focused runs too.
    // Scope, source authority and test/cell coverage still decide qualification;
    // knowing which work was selected does not mean it completed.
    let gates_expected = planned_tags.len();
    // A host-inapplicable node is NEVER in `gates`: that array is the executed
    // PASS/FAIL list, and a node that did not run belongs in neither state. It
    // is carried in its own typed field instead, with the observation behind the
    // judgement, so no reader can mistake absence for coverage.
    let intentional_skipped_nodes: Vec<serde_json::Value> = host_inapplicable
        .iter()
        .map(|n| {
            serde_json::json!({
                "name": n.tag,
                "reason": validate_plan::HOST_INAPPLICABLE_REASON,
                "capability": n.capability.value(),
                "evidence": n.evidence,
            })
        })
        .collect();
    let gates: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|outcome| ledger_gate_with_attempts(outcome, attempts))
        .collect();
    // Nodes that were re-run, and nodes for which no completion payload ever
    // arrived. The second list is the verdict-capture population: a node that
    // reported nothing established nothing, and it is deliberately NOT retried
    // on that ground alone, so it must at least be counted here.
    let retried_nodes: Vec<&str> = {
        let mut names: Vec<&str> = attempts
            .iter()
            .filter(|a| a.attempt > 1)
            .map(|a| a.tag.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let unreported_attempt_nodes: Vec<&str> = {
        let mut names: Vec<&str> = attempts
            .iter()
            .filter(|a| !a.reported)
            .map(|a| a.tag.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let accounted: BTreeSet<&str> = outcomes
        .iter()
        .map(|o| o.tag.as_str())
        .chain(skipped.iter().map(String::as_str))
        .chain(host_inapplicable.iter().map(|n| n.tag.as_str()))
        .collect();
    let unaccounted_nodes: Vec<&str> = planned_tags
        .iter()
        .map(String::as_str)
        .filter(|tag| !accounted.contains(tag))
        .collect();
    let environment_run_id = std::env::var("E2E_RUN_ID")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let run_id = cell_results
        .map(|results| results.run_id.as_str())
        .or_else(|| coverage.get("run_id").and_then(serde_json::Value::as_str))
        .or(environment_run_id.as_deref());
    let record = serde_json::json!({
        "schema_version": ledger_schema,
        "repo": "hermit",
        "producer": LEDGER_PRODUCER,
        "admission": ctx.admission,
        // Immutable-row identity. `corrects` is null here; a correcting row
        // repeats this shape with `corrects` set to the id it supersedes.
        "record_id": record_id,
        "corrects": serde_json::Value::Null,
        "run_id": run_id,
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
        "git_depth": ctx.git_depth,
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
        "checks": gate_records,
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
        // Retry ROUNDS spent on retry-eligible failures. This is not the
        // historical `env_block_retries` population: bound kills, measured
        // instability, and always-eligible failures can all start a round now.
        // A green that needed one must remain distinguishable from a first-pass
        // green without calling every retry an environmental block.
        "retry_rounds": ctx.retry_rounds,
        // WHICH nodes were retried, not merely how many rounds the lane spent.
        // A round count alone cannot answer "what flaked?", so a per-node rate
        // was not computable from any row written before this field.
        "retried_nodes": retried_nodes,
        // Nodes that produced no completion payload on some attempt. Separate
        // from a failure on purpose: the run learned nothing about these, and
        // the two need opposite fixes.
        "unreported_attempt_nodes": unreported_attempt_nodes,
        // LIBTEST counts aggregated from the runner's typed step outcomes before
        // verbosity filters their human-facing presentation.
        // `null` is UNKNOWN and stays UNKNOWN: the receipt publisher fails closed
        // rather than turning missing evidence into a zero or a pass. These are
        // the counts every downstream `is_clean_full_pass` predicate keys on, so
        // a row without them is a NON-VERDICT, not a green.
        "executed_tests": ctx.executed_tests,
        "passed_tests": ctx.passed_tests,
        "filtered_tests": ctx.filtered_tests,
        "gates_run": gate_records,
        "gates_expected": gates_expected,
        "validation_complete": validation_complete,
        "product_result_node_count": classification.product_result_nodes.len(),
        "product_result_nodes": classification.product_result_nodes,
        "understood_infrastructure_failure_nodes": classification.understood_infrastructure_failure_nodes,
        "understood_prerequisite_failure_nodes": classification.understood_prerequisite_failure_nodes,
        "no_result_nodes": classification.no_result_nodes,
        "skipped_nodes": skipped.len() + intentional_skipped_nodes.len(),
        // Typed pre-spawn omissions: nodes this MACHINE provably cannot run.
        // The reason vocabulary is closed on BOTH sides. The parent consumer
        // (ci-hub/validate/gate_completeness.py, ci-hub/lib/qualifying_receipt.rs)
        // admits only `empty-manifest-bucket`, so a row carrying
        // `host-inapplicable` is NOT a qualifying receipt until the owner opts
        // that reason in. Recording the omission honestly is what costs the
        // receipt; it is not a way to buy one.
        "intentional_skipped_nodes": intentional_skipped_nodes,
        // Nodes that never ran because something they depend on failed. Named,
        // not just counted, so a reader can tell the two kinds of absence apart.
        "dependency_skipped_nodes": skipped,
        // Planned nodes with NO terminal result and no recorded reason —
        // computed against the planned tag set rather than asserted empty, so a
        // deadline cut or a lane that never started is visible instead of
        // vanishing.
        "unaccounted_nodes": unaccounted_nodes,
        // A timeout is a RESULT, so it is recorded rather than dropped, and it is
        // named so a reader can separate "the tree is broken" from "a gate blew
        // its budget". Operator interrupts never reach this function at all.
        "timed_out_nodes": timed_out,
        // NODE counts, deliberately NOT named executed_tests/filtered_tests: a
        // schema<5 consumer keys is_clean_full_pass on those libtest-count names,
        // and a ~47-NODE DAG run must never be readable as a 47-TEST pass. The
        // counted receipt consumes the explicit test fields above rather than
        // treating this node count as test evidence.
        "executed_nodes": executed_nodes,
        // Exact outer plan identity. `profile=full` does not imply the nodes in
        // quick or super, so the receipt carries the names it actually planned
        // instead of asking readers to infer a set from the profile label.
        "planned_node_count": planned_tags.len(),
        "planned_nodes": planned_tags,
        "real_seconds": wall_s,
        "log_file": log_file,
        "coverage": coverage,
        "cell_results": cell_results.map(|results| &results.evidence),
        "gates": gates,
    });
    let typed = match serde_json::from_value::<HistoryRow>(record.clone()) {
        Ok(typed) => typed,
        Err(error) => {
            eprintln!(
                "validate: warning: generated ledger row does not match the shared HistoryRow: {error}"
            );
            return;
        }
    };
    if typed.retry_rounds() != Ok(Some(ctx.retry_rounds)) {
        eprintln!(
            "validate: warning: generated ledger row has malformed HistoryRow retry_rounds"
        );
        return;
    }
    if typed.executed_nodes() != Ok(Some(executed_nodes)) {
        eprintln!(
            "validate: warning: generated ledger row has malformed HistoryRow executed_nodes"
        );
        return;
    }
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    let explicit = std::env::var(LEDGER_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .is_some_and(|value| Path::new(&value) == ledger);
    if !explicit && ledger.file_name().is_some_and(|name| name == "ledger") {
        let configured_tool_root = std::env::var_os(TOOL_ROOT_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let Some(adapter) = validate_history::canonical_ledger_adapter(
            ledger,
            configured_tool_root.as_deref(),
        ) else {
            eprintln!("validate: warning: canonical ledger root has no parent: {}", ledger.display());
            return;
        };
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

fn establish_cell_host_facts(nested: bool) -> Result<(), String> {
    if nested {
        for name in [E2E_MACHINE_SHORTNAME_ENV, E2E_KERNEL_VERSION_ENV] {
            if std::env::var(name)
                .ok()
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(format!(
                    "{name} was not forwarded into the pinned root; refusing to record the container hostname as the measurement machine"
                ));
            }
        }
        return Ok(());
    }

    let machine_shortname = short_hostname();
    if machine_shortname == "unknown" || machine_shortname.contains('/') {
        return Err(format!(
            "cannot establish a short machine name for cell results: {machine_shortname:?}"
        ));
    }
    let kernel_version = sh("uname", &["-r"])
        .filter(|value| !value.trim().is_empty())
        .ok_or("cannot establish kernel_version for cell results")?;
    // SAFETY: validation owns these process-wide values before the DAG starts;
    // worker threads are created only after plan construction completes.
    unsafe {
        std::env::set_var(E2E_MACHINE_SHORTNAME_ENV, machine_shortname);
        std::env::set_var(E2E_KERNEL_VERSION_ENV, kernel_version);
    }
    Ok(())
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
    /// A completed gate could not determine its condition.
    NoResult,
    /// Admission control declined to run: dirty tree, stale base, unplannable
    /// profile, uncapped node, no boxing, no durable log, bad arguments.
    Refused,
    /// An operator stop. A NO-RESULT, not a failure.
    Interrupted,
    /// `--show-plan` or `--write-constructed-dag`: nothing was executed by design.
    PlanOnly,
    /// A prior passing record for this exact tree was reused.
    CacheHit,
    SelfTest,
    /// `--help`; the usage text IS the output.
    Help,
}

const FINAL_VALIDATE_STATUS_PREFIX: &str = "FINAL_VALIDATE_STATUS: ";
const COULD_NOT_RUN_EXIT_CODE: u8 = NO_RESULT_EXIT_CODE as u8;
const VALIDATE_SERVICE_RESULT_PATH_ENV: &str = "VALIDATE_SERVICE_RESULT_PATH";

fn final_validate_status(verdict: Verdict) -> Option<FinalValidateStatus> {
    match verdict {
        Verdict::Pass | Verdict::SelfTest | Verdict::CacheHit => {
            Some(FinalValidateStatus::Passed)
        }
        Verdict::Fail => Some(FinalValidateStatus::Failed),
        Verdict::NoResult | Verdict::Refused | Verdict::Interrupted => {
            Some(FinalValidateStatus::CouldNotRun)
        }
        Verdict::PlanOnly | Verdict::Help => None,
    }
}

fn final_validate_status_from_output(output: &str) -> Result<Option<FinalValidateStatus>, String> {
    // `next_back()`, not `last()`: both yield the final matching line, but `last()`
    // walks the whole iterator to get there and clippy refuses it on a
    // double-ended iterator. The LAST occurrence is deliberate -- a nested run can
    // emit the prefix more than once and the outermost status is the one that counts.
    let Some(value) = output
        .lines()
        .filter_map(|line| line.strip_prefix(FINAL_VALIDATE_STATUS_PREFIX))
        .next_back()
    else {
        return Ok(None);
    };
    match value {
        "PASSED" => Ok(Some(FinalValidateStatus::Passed)),
        "FAILED" => Ok(Some(FinalValidateStatus::Failed)),
        "COULD_NOT_RUN" => Ok(Some(FinalValidateStatus::CouldNotRun)),
        other => Err(format!("unknown final validate status {other:?}")),
    }
}

impl Verdict {
    fn marker(self) -> &'static str {
        match self {
            Verdict::Pass | Verdict::SelfTest | Verdict::CacheHit => "✅",
            Verdict::Fail => "❌",
            Verdict::NoResult => "⏹",
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
            Verdict::NoResult => "NO-RESULT",
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
    /// Operator action rendered after the common footer and immediately before
    /// the final machine-readable status. A refusal's pasteable recovery command
    /// belongs here so it stays adjacent to the conclusion without violating the
    /// status-line ordering contract.
    epilogue: Vec<String>,
    profile: String,
    selection_mode: Option<String>,
    commit: String,
    nodes_executed: usize,
    nodes_failed: usize,
    nodes_skipped: usize,
    /// Planned nodes withheld because the MACHINE provably cannot run them.
    /// Counted separately from `nodes_executed` and `nodes_skipped` so the
    /// one-line accounting can never read as though everything planned ran.
    nodes_host_inapplicable: usize,
    /// Aggregate from typed step outcomes. `None` is unknown, never zero.
    executed_tests: Option<i64>,
    /// Exact terminal passes from those same typed framework outcomes. This is
    /// never reconstructed from the validation verdict or executed-test count.
    passed_tests: Option<i64>,
    /// Individual test ids that failed and then passed, with the retry grants
    /// that followed their failed attempts. Rendered even on a green run.
    flaky: Vec<TestIdRetry>,
    /// Individual test ids that failed FINALLY, after any retries were exhausted.
    failed_ids: Vec<TestIdRetry>,
    /// Failed DAG nodes for which no individual failing test id was emitted.
    /// Kept separate so a node tag is never presented as a test id.
    failed_nodes_without_test_ids: Vec<String>,
    /// Exact count of retry grants, read from outer scheduler retry classes and
    /// inner per-cell attempt rows. This is deliberately not `retried_nodes`,
    /// which includes successful peers re-run as part of a lane.
    retry_occurrences: usize,
    /// Whether every applicable producer supplied individual typed results.
    /// False means the summary must say UNKNOWN rather than claim a clean zero.
    individual_test_results_complete: bool,
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
    /// Bookkeeping performed after the validation verdict was finalized.
    /// Failure remains a loud command error but cannot rewrite that verdict.
    scorecard_writeback: Option<ScorecardWriteback>,
}

impl RunSummary {
    fn new(verdict: Verdict, exit_code: u8, profile: &str, detail: Vec<String>) -> Self {
        let exit_code = final_validate_status(verdict)
            .map(|status| u8::try_from(status.exit_code()).expect("fixed exit fits u8"))
            .unwrap_or(exit_code);
        RunSummary {
            verdict,
            exit_code,
            detail,
            epilogue: Vec::new(),
            profile: profile.to_string(),
            selection_mode: None,
            commit: git_sha(),
            nodes_executed: 0,
            nodes_failed: 0,
            nodes_skipped: 0,
            nodes_host_inapplicable: 0,
            executed_tests: None,
            passed_tests: None,
            flaky: Vec::new(),
            failed_ids: Vec::new(),
            failed_nodes_without_test_ids: Vec::new(),
            retry_occurrences: 0,
            individual_test_results_complete: false,
            wall_s: None,
            jobs: None,
            log: None,
            ledger: None,
            cpu_wall: None,
            scorecard_writeback: None,
        }
    }
    /// Admission control declined. `what` names the gate, `why` the reason.
    fn refused(exit_code: u8, profile: &str, what: &str, why: Vec<String>) -> Self {
        let mut detail = vec![format!("refused by: {what}")];
        detail.extend(why);
        RunSummary::new(Verdict::Refused, exit_code, profile, detail)
    }

    fn with_epilogue(mut self, epilogue: Vec<String>) -> Self {
        self.epilogue = epilogue;
        self
    }
}

fn cache_hit_run_summary(
    hit: &validate_history::CacheHit,
    profile: &str,
    selection_mode: &str,
    tree: &str,
    ledger: &Path,
    service_result_requested: bool,
) -> Result<RunSummary, String> {
    let exact_counts = hit.exact_current_pass_counts();
    if service_result_requested && exact_counts.is_none() {
        return Err(format!(
            "cached row cannot publish a current validation service result: requires current-schema Rust producer evidence with positive executed_nodes and exact positive executed_tests == passed_tests; got schema_version={:?}, producer={:?}, executed_nodes={:?}, executed_tests={:?}, passed_tests={:?}",
            hit.schema_version,
            hit.producer,
            hit.executed_nodes,
            hit.executed_tests,
            hit.passed_tests,
        ));
    }

    let mut summary = RunSummary::new(
        Verdict::CacheHit,
        0,
        profile,
        vec![
            format!(
                "reused the passing record from {} (commit {}, producer {}), keyed on tree {tree}",
                hit.finished_at, hit.commit, hit.producer
            ),
            format!(
                "that run recorded {} {} executed with satisfied gate coverage; --ignore-cache forces a real run",
                hit.executed, hit.executed_unit
            ),
        ],
    );
    summary.selection_mode = Some(selection_mode.into());
    summary.ledger = Some(ledger.to_path_buf());
    if service_result_requested {
        let (nodes, executed, passed) = exact_counts.expect("checked above");
        summary.nodes_executed = usize::try_from(nodes).map_err(|_| {
            format!("cached executed_nodes {nodes} does not fit this platform")
        })?;
        summary.executed_tests = Some(executed);
        summary.passed_tests = Some(passed);
    }
    Ok(summary)
}

fn unavailable_invocation_lock_summary(profile: &str, error: String) -> RunSummary {
    RunSummary::refused(
        3,
        profile,
        "the per-checkout invocation lock",
        vec![format!(
            "cannot establish per-checkout exclusion: {error}; refusing rather than running two validates against shared target output"
        )],
    )
}

const SUMMARY_FLAKY_HEADING: &str =
    "⚠️  FLAKY — these test ids passed only after a retry:";

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestIdRetry {
    node: String,
    id: String,
    retry_classes: Vec<RetryClass>,
    inner_retry_occurrences: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestAttemptObservation {
    node: String,
    attempt: usize,
    id: String,
    passed: bool,
    /// Attempts made inside the test runner before this terminal result.
    inner_attempts: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TestIdSummary {
    recovered: Vec<TestIdRetry>,
    failed: Vec<TestIdRetry>,
    failed_nodes_without_test_ids: Vec<String>,
    retry_occurrences: usize,
}

/// Render the complete id list. Failure output is complete by contract, and a
/// truncated summary would not answer which tests failed.
fn summary_id_list(ids: &[String]) -> Vec<String> {
    ids.iter().map(|id| format!("     - {id}")).collect()
}

/// Retry grants attributable to one individual test id's failed observations.
///
/// ⚠️ READ `attempts[].retry_class`, NEVER `retried_nodes`. THIS IS NOT A STYLE
/// PREFERENCE AND THE WRONG FIELD PRODUCES A PLAUSIBLE NUMBER. `retried_nodes` is
/// every node with `attempt > 1`, and an environmental retry round RE-RUNS THE
/// WHOLE LANE -- so it fills with nodes that never failed. Measured on real rows:
///
/// ```text
///     2026-08-25T05:08:07Z   retried_nodes = 37   env_block_retries = 2
///     2026-08-25T02:27:14Z   retried_nodes = 22   env_block_retries = 1
/// ```
///
/// At most a handful of those 37 failed; the rest were re-run because something
/// else did. A retry count built from that field is roughly 5x too high, and it
/// looks entirely reasonable, which is why nothing would catch it. `retry_class`
/// is set only on an attempt for which a retry was actually GRANTED, per node,
/// and carries the closed class the retry line printed. Changing evidence is
/// retained separately on the attempt as `retry_detail`.
fn retry_classes_for_test(
    observations: &[TestAttemptObservation],
    attempts: &[NodeAttempt],
) -> Vec<RetryClass> {
    observations
        .iter()
        .filter(|observation| !observation.passed)
        .filter_map(|observation| {
            let outer_class = attempts
                .iter()
                .find(|attempt| {
                    attempt.tag == observation.node && attempt.attempt == observation.attempt
                })
                .and_then(|attempt| attempt.retry_class);
            outer_class.or_else(|| {
                observations
                    .iter()
                    .any(|later| {
                        later.node == observation.node && later.attempt > observation.attempt
                    })
                    .then_some(RetryClass::AlwaysEligible)
            })
        })
        .collect()
}

fn inner_retry_occurrences_for_test(
    observations: &[TestAttemptObservation],
    attempts: &[NodeAttempt],
) -> usize {
    observations
        .iter()
        .filter(|observation| !observation.passed)
        .filter(|observation| {
            observations.iter().any(|later| {
                later.node == observation.node && later.attempt > observation.attempt
            })
        })
        .filter(|observation| {
            !attempts.iter().any(|attempt| {
                attempt.tag == observation.node
                    && attempt.attempt == observation.attempt
                    && attempt.retry_class.is_some()
            })
        })
        .count()
}

/// Read terminal nextest results from the exact scheduler attempt that received them.
///
/// A completed nextest step with no structured result is an error, not an empty
/// test population. Human output remains presentation and cannot manufacture a
/// functional test result.
fn nextest_test_observations(
    attempts: &[NodeAttempt],
    nextest_nodes: &BTreeSet<String>,
) -> (Vec<TestAttemptObservation>, Vec<String>) {
    let mut observations = Vec::new();
    let mut errors = Vec::new();
    for attempt in attempts {
        if !nextest_nodes.contains(&attempt.tag)
            || !attempt.reported
            || attempt.execution != AttemptExecution::Completed
        {
            continue;
        }
        let Some(results) = &attempt.test_results else {
            errors.push(format!(
                "individual nextest results are UNKNOWN for node {} attempt {}: the controlled runner published no typed test-result rows",
                attempt.tag, attempt.attempt
            ));
            continue;
        };
        for result in results {
            let dagrun::TestResult {
                id,
                passed,
                attempts: inner_attempts,
            } = result;
            let Ok(inner_attempts) = usize::try_from(*inner_attempts) else {
                errors.push(format!(
                    "individual nextest result {} for node {} has an attempt count too large for this process",
                    id, attempt.tag
                ));
                continue;
            };
            observations.push(TestAttemptObservation {
                node: attempt.tag.clone(),
                attempt: attempt.attempt,
                id: id.clone(),
                passed: *passed,
                inner_attempts,
            });
        }
    }
    (observations, errors)
}

const DBT_PARITY_NODE: &str = "test.dbt_parity";

/// Parse one result emitted by the standalone DBT parity matrix.
///
/// `run_matrix.py` owns the stable case name `backend-parity/<case>`; the
/// suffix records the backend and mode selected by the DAG node. Diagnostic,
/// gap, blocked, and malformed lines are not individual test outcomes.
fn dbt_parity_test_observation(rest: &str) -> Option<(bool, String)> {
    let rest = rest.trim_start();
    let (passed, result) = if let Some(result) = rest.strip_prefix("PASS ") {
        (true, result)
    } else {
        (false, rest.strip_prefix("FAIL ")?)
    };
    let (identity, _detail) = result.split_once(':')?;
    let case = identity.strip_prefix("dbt/")?;
    if case.is_empty()
        || !case
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
    {
        return None;
    }
    Some((passed, format!("backend-parity/{case} [dbt/strict]")))
}

/// Recover DBT parity case results from the durable scheduler log.
///
/// Each scheduler START begins a new attempt. Keeping that boundary means a
/// failed case that passes on retry remains visible as a recovered test id,
/// while a node that dies before emitting any case result still has no invented
/// id and remains in `failed_nodes_without_test_ids`.
fn dbt_parity_test_observations(log: &str) -> Vec<TestAttemptObservation> {
    let mut attempt = 0;
    let mut seen: BTreeSet<(usize, String)> = BTreeSet::new();
    let mut observations = Vec::new();
    for line in log.lines() {
        let Some(after_open) = line.strip_prefix('[') else { continue };
        let Some((node, rest)) = after_open.split_once(']') else { continue };
        if node != DBT_PARITY_NODE {
            continue;
        }
        if rest.trim_start().starts_with("▶ START") {
            attempt += 1;
            continue;
        }
        let Some((passed, id)) = dbt_parity_test_observation(rest) else { continue };
        let attempt = attempt.max(1);
        if seen.insert((attempt, id.clone())) {
            observations.push(TestAttemptObservation {
                node: DBT_PARITY_NODE.to_string(), attempt, id, passed, inner_attempts: 1,
            });
        }
    }
    observations
}

fn collect_e2e_result_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(path)
        .map_err(|error| format!("cannot read per-cell result root {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read per-cell result entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot classify {}: {error}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_e2e_result_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == "results.jsonl" {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn e2e_test_observations(root: &Path) -> Result<Vec<TestAttemptObservation>, String> {
    let mut files = Vec::new();
    collect_e2e_result_files(root, &mut files)?;
    files.sort();
    let mut seen = BTreeSet::new();
    let mut observations = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: serde_json::Value = serde_json::from_str(line).map_err(|error| {
                format!("{}:{} malformed result row: {error}", file.display(), line_number + 1)
            })?;
            let field = |name: &str| {
                row.get(name)
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("{}:{} has no {name}", file.display(), line_number + 1))
            };
            let lane = field("lane")?;
            let category = field("category")?;
            let test = field("test")?;
            let mode = field("mode")?;
            let backend = field("backend")?;
            let outcome = field("outcome")?;
            let attempt = row.get("attempt").and_then(serde_json::Value::as_u64).unwrap_or(1);
            let attempt = usize::try_from(attempt)
                .map_err(|_| format!("{}:{} attempt does not fit usize", file.display(), line_number + 1))?;
            if attempt == 0 {
                return Err(format!("{}:{} attempt must be positive", file.display(), line_number + 1));
            }
            let id = format!("{test} [{backend}/{mode}]");
            if !seen.insert((lane.to_string(), id.clone(), attempt)) {
                return Err(format!(
                    "{}:{} duplicates test id {id} attempt {attempt}",
                    file.display(), line_number + 1
                ));
            }
            let group = match lane {
                "portable" => "e2e",
                "privileged" => "privileged-e2e",
                _ => {
                    return Err(format!(
                        "{}:{} has unrecognized lane {lane}",
                        file.display(), line_number + 1
                    ));
                }
            };
            let category = category.replace('-', "_");
            observations.push(TestAttemptObservation {
                node: format!("{group}.manifest_{category}"),
                attempt,
                id,
                passed: outcome == "PASS",
                inner_attempts: 1,
            });
        }
    }
    observations.sort_by(|left, right| {
        (&left.id, left.attempt, &left.node).cmp(&(&right.id, right.attempt, &right.node))
    });
    Ok(observations)
}

fn test_id_summary(
    mut observations: Vec<TestAttemptObservation>,
    attempts: &[NodeAttempt],
    failed_nodes: &BTreeSet<String>,
) -> TestIdSummary {
    observations.sort_by(|left, right| {
        (&left.node, &left.id, left.attempt).cmp(&(&right.node, &right.id, right.attempt))
    });
    // A test id is only unique inside the DAG node that executed it. Grouping
    // solely by id lets a passing peer node replace a failing node's terminal
    // observation (or vice versa) according to lexical node order. Keep the
    // producer's complete identity through classification and rendering.
    let mut by_node_and_id: BTreeMap<(String, String), Vec<TestAttemptObservation>> =
        BTreeMap::new();
    for observation in observations {
        by_node_and_id
            .entry((observation.node.clone(), observation.id.clone()))
            .or_default()
            .push(observation);
    }
    let mut recovered = Vec::new();
    let mut failed = Vec::new();
    let mut failed_nodes_with_test_ids = BTreeSet::new();
    let mut unclassified_outer_retry_occurrences = 0;
    let mut test_runner_retry_occurrences = 0;
    for ((node, id), observations) in by_node_and_id {
        let Some(last) = observations.last() else { continue };
        unclassified_outer_retry_occurrences +=
            inner_retry_occurrences_for_test(&observations, attempts);
        let inner_retry_occurrences = observations
            .iter()
            .map(|observation| observation.inner_attempts.saturating_sub(1))
            .sum::<usize>();
        test_runner_retry_occurrences += inner_retry_occurrences;
        let retry_classes = retry_classes_for_test(&observations, attempts);
        let was_retried = !retry_classes.is_empty() || inner_retry_occurrences > 0;
        let item = TestIdRetry { node, id, retry_classes, inner_retry_occurrences };
        if last.passed {
            if was_retried {
                recovered.push(item);
            }
        } else if failed_nodes.contains(&last.node) {
            failed_nodes_with_test_ids.insert(last.node.clone());
            failed.push(item);
        }
    }
    TestIdSummary {
        recovered,
        failed,
        failed_nodes_without_test_ids: failed_nodes
            .difference(&failed_nodes_with_test_ids)
            .cloned()
            .collect(),
        retry_occurrences: attempts.iter().filter(|attempt| attempt.retry_class.is_some()).count()
            + unclassified_outer_retry_occurrences
            + test_runner_retry_occurrences,
    }
}

fn render_test_id_retry(item: &TestIdRetry) -> String {
    let retries = item.retry_classes.len() + item.inner_retry_occurrences;
    let classes = if item.retry_classes.is_empty() {
        String::new()
    } else {
        format!(
            ": {}",
            item.retry_classes
                .iter()
                .map(|class| class.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    };
    format!(
        "{} (node {})  ({retries} retr{}{})",
        item.id,
        item.node,
        if retries == 1 { "y" } else { "ies" },
        classes
    )
}

/// The ONE summary renderer. Called from exactly one place.
///
/// `started` is the process's own start instant, used only when a path stopped
/// before cleanup could take the authoritative measurement.
fn run_summary_lines(s: &RunSummary, started: std::time::Instant) -> Vec<String> {
    if s.verdict == Verdict::Help {
        return Vec::new();
    }
    let validation_exit_code = final_validate_status(s.verdict)
        .map(|status| u8::try_from(status.exit_code()).expect("fixed exit fits u8"))
        .unwrap_or(s.exit_code);
    let mut lines = vec![
        String::new(),
        if validation_exit_code == s.exit_code {
            format!(
                "{} validate {} (exit {}) — profile {} @ {}",
                s.verdict.marker(),
                s.verdict.word(),
                validation_exit_code,
                s.profile,
                s.commit
            )
        } else {
            format!(
                "{} validate {} (validation exit {}; command exit {}) — profile {} @ {}",
                s.verdict.marker(),
                s.verdict.word(),
                validation_exit_code,
                s.exit_code,
                s.profile,
                s.commit
            )
        },
    ];
    for line in &s.detail {
        lines.push(format!("   {line}"));
    }
    // ---- the four-part end-of-run summary (owner directive 2026-08-26) ----
    //
    // ⚠️ THE FLAKY BLOCK IS RENDERED ON A PASSING RUN. That is the whole point and
    // it is the part that gets dropped, because on a green run there is nothing
    // demanding attention and the block looks like noise. A test that failed and
    // then passed is the only warning anyone gets before it fails for real.
    if !s.flaky.is_empty() {
        lines.push(String::new());
        lines.push(format!("   {SUMMARY_FLAKY_HEADING}"));
        let ids: Vec<String> = s.flaky.iter().map(render_test_id_retry).collect();
        lines.extend(summary_id_list(&ids));
        lines.push(format!("     {} test id(s) recovered", s.flaky.len()));
    }
    if !s.failed_ids.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "   ❌ FAILURE — {} test id(s) failed and did NOT recover on retry:",
            s.failed_ids.len()
        ));
        let ids: Vec<String> = s.failed_ids.iter().map(render_test_id_retry).collect();
        lines.extend(summary_id_list(&ids));
    }
    if !s.failed_nodes_without_test_ids.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "   ❌ FAILURE — {} node(s) failed without emitting an individual test id:",
            s.failed_nodes_without_test_ids.len()
        ));
        lines.extend(summary_id_list(&s.failed_nodes_without_test_ids));
    }
    if s.wall_s.is_some() && s.nodes_executed > 0 && s.individual_test_results_complete {
        lines.push(format!(
            "   retries: {} occurrence(s) recorded from scheduler and per-cell attempts",
            s.retry_occurrences
        ));
    }
    if s.wall_s.is_some() && s.nodes_executed > 0 && !s.individual_test_results_complete {
        lines.push(
            "   retries and individual test results: UNKNOWN — one or more producers supplied no typed result"
                .to_string(),
        );
    }
    // ⚠️ A CLEAN RUN SAYS SO, RATHER THAN SAYING NOTHING. With both blocks above
    // conditional and no else, a run with nothing to report rendered IDENTICALLY
    // to a run whose retry accounting produced nothing — the two are the same
    // bytes, so absence was readable as a result. That is the defect shape this
    // project keeps paying for, and it lands specifically on the reader the
    // trailing section exists for: someone scanning for a flaky warning cannot
    // tell "there were none" from "the question was never asked".
    //
    // Only when a DAG actually ran. Before that there is genuinely nothing to
    // have counted, and claiming otherwise would be the same error inverted.
    if s.flaky.is_empty()
        && s.failed_ids.is_empty()
        && s.failed_nodes_without_test_ids.is_empty()
        && s.retry_occurrences == 0
        && s.individual_test_results_complete
        && s.wall_s.is_some()
        && s.nodes_executed > 0
    {
        lines.push(String::new());
        lines.push(
            "   no retries, no flaky tests, and no failed test ids: every executed node passed first time"
                .to_string(),
        );
    }
    // Node accounting is printed whenever a DAG ran, and deliberately printed as
    // an explicit zero when one did not, so "no nodes ran" is a stated fact
    // rather than an absent line a reader has to interpret.
    match s.wall_s {
        Some(wall) => lines.push(format!(
            "   nodes: {} executed, {} failed, {} skipped{} in {}{}",
            s.nodes_executed,
            s.nodes_failed,
            s.nodes_skipped,
            if s.nodes_host_inapplicable == 0 {
                String::new()
            } else {
                format!(
                    ", {} host-inapplicable (NOT RUN, NOT passed)",
                    s.nodes_host_inapplicable
                )
            },
            human_duration(wall),
            s.jobs.map(|j| format!(" at -j {j}")).unwrap_or_default()
        )),
        None => lines.push("   nodes: none executed (stopped before the DAG ran)".into()),
    }
    match &s.log {
        Some(p) => lines.push(format!("   durable log: {}", p.display())),
        None => lines.push("   durable log: (none — stopped before one was opened)".into()),
    }
    if let Some(p) = &s.ledger {
        lines.push(format!("   ledger: {}", p.display()));
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
    lines.push(format!(
        "   {}",
        validate_runtime::cpu_wall_line(human_duration, wall, user, sys, host_cpus)
    ));
    lines.extend(s.epilogue.iter().cloned());
    if let Some(status) = final_validate_status(s.verdict) {
        // LAST by contract. A wrapper, guest, fixture or quoted diagnostic may
        // have written an earlier lookalike to the same channel. This line is the
        // validation verdict; the versioned result records a distinct post-verdict
        // write-back failure when the command exit does not match that verdict.
        lines.push(format!("{FINAL_VALIDATE_STATUS_PREFIX}{}", status.as_str()));
    }
    lines
}

fn print_run_summary(s: &RunSummary, started: std::time::Instant) {
    for line in run_summary_lines(s, started) {
        println!("{line}");
    }
}

fn write_validation_service_result(path: &Path, summary: &RunSummary) -> Result<(), String> {
    use std::io::Write;

    let Some(status) = final_validate_status(summary.verdict) else {
        return Ok(());
    };
    let result = ValidationServiceResult {
        schema_version: hermit_manifest_plan::service_result::SCHEMA_VERSION,
        commit: summary.commit.clone(),
        profile: summary.profile.clone(),
        selection_mode: summary.selection_mode.clone(),
        final_validate_status: status,
        exit_code: i32::from(summary.exit_code),
        executed_nodes: u64::try_from(summary.nodes_executed)
            .map_err(|_| "validation-service-result-executed_nodes exceeds u64".to_string())?,
        executed_tests: summary.executed_tests,
        passed_tests: summary.passed_tests,
        scorecard_writeback: summary.scorecard_writeback.clone(),
    }
    .validated()?;
    let bytes = serde_json::to_vec(&result)
        .map_err(|error| format!("cannot encode validation service result: {error}"))?;
    let parent = path.parent().ok_or_else(|| {
        format!(
            "cannot publish validation service result to {}: path has no parent",
            path.display()
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "cannot create validation service result beside {}: {error}",
            path.display()
        )
    })?;
    temporary
        .write_all(&[bytes.as_slice(), b"\n"].concat())
        .and_then(|()| temporary.flush())
        .map_err(|error| format!("cannot write validation service result: {error}"))?;
    temporary.persist_noclobber(path).map_err(|error| {
        format!(
            "cannot publish validation service result to {} without replacing an existing result: {error}",
            path.display()
        )
    })?;
    Ok(())
}

fn publish_validation_service_result(
    path: Option<&Path>,
    summary: &RunSummary,
) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    write_validation_service_result(path, summary)
}

/// `--probe-host-capability <name>`: report THIS machine's verdict for one
/// capability and exit, printing `PRESENT\t<evidence>` or `ABSENT\t<evidence>`.
///
/// A read-only query seam, in the same class as `--show-plan`: it runs no gate,
/// writes no ledger, and applies no label. It exists so a consumer that is not
/// this driver can reuse the SAME probe. Today that consumer is
/// `target/debug/test-harness`, which withholds a manifest CELL the machine cannot run
/// the way the driver withholds a NODE. Exposing the existing probe was the
/// alternative to writing a second one, and two probes for one question would
/// eventually disagree.
///
/// An unrecognized name exits 2 rather than answering: the vocabulary is closed
/// in [`validate_plan::HostCapability`], and inventing an answer for a name
/// nobody defined is exactly how a bogus reason to skip work would appear.
///
/// Returns `None` when the flag is absent, so ordinary parsing proceeds.
fn probe_host_capability_query() -> Option<u8> {
    let mut argv = std::env::args().skip(1);
    let name = loop {
        let arg = argv.next()?;
        if let Some(value) = arg.strip_prefix("--probe-host-capability=") {
            break value.to_string();
        }
        if arg == "--probe-host-capability" {
            match argv.next() {
                Some(value) => break value,
                None => {
                    eprintln!("validate: --probe-host-capability needs a capability name");
                    return Some(2);
                }
            }
        }
    };
    let Some(capability) = validate_plan::HostCapability::from_value(&name) else {
        eprintln!(
            "validate: unknown host capability '{name}'; the vocabulary is closed \
             (hermit_manifest_plan::host_capability::HostCapability) and an unrecognized name is refused \
             rather than answered"
        );
        return Some(2);
    };
    let verdict = validate_plan::probe_host_capability(capability);
    println!(
        "{}\t{}",
        if verdict.present { "PRESENT" } else { "ABSENT" },
        verdict.evidence
    );
    Some(0)
}

fn cargo_manifest_boundary(root: &Path) -> PathBuf {
    root.ancestors()
        .filter(|candidate| candidate.join("Cargo.toml").is_file())
        .last()
        .unwrap_or(root)
        .to_path_buf()
}

fn path_is_outside(path: &Path, boundary: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let boundary =
        std::fs::canonicalize(boundary).unwrap_or_else(|_| boundary.to_path_buf());
    !path.starts_with(boundary)
}

fn create_safe_cache(root: &Path, parent: Option<&Path>) -> Result<PathBuf, String> {
    let boundary = cargo_manifest_boundary(root);
    let mut bases = Vec::new();
    if let Some(parent) = parent {
        bases.push(parent.join("ignored/validate/cache"));
    }
    if let Some(outside) = boundary.parent() {
        bases.push(outside.join("ignored/validate/cache"));
    }
    bases.push(std::env::temp_dir().join("hermit-validate-cache"));
    bases.dedup();

    let mut failures = Vec::new();
    for base in bases {
        if !path_is_outside(&base, &boundary) {
            failures.push(format!(
                "{} is not outside Cargo workspace {}",
                base.display(),
                boundary.display()
            ));
            continue;
        }
        let created = match std::fs::symlink_metadata(&base) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                failures.push(format!("cache path {} is a symlink", base.display()));
                continue;
            }
            Ok(metadata) if !metadata.is_dir() => {
                failures.push(format!("cache path {} is not a directory", base.display()));
                continue;
            }
            Ok(metadata) => {
                let mode = metadata.mode() & 0o777;
                let owner = metadata.uid();
                let effective_uid = unsafe { libc::geteuid() };
                if owner != effective_uid {
                    failures.push(format!(
                        "cache path {} is owned by uid {owner}, not effective uid {effective_uid}",
                        base.display()
                    ));
                    continue;
                }
                if mode & 0o022 != 0 {
                    failures.push(format!(
                        "cache path {} has unsafe mode {mode:04o}",
                        base.display()
                    ));
                    continue;
                }
                false
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(error) = std::fs::create_dir_all(&base) {
                    failures.push(format!("cannot create {}: {error}", base.display()));
                    continue;
                }
                match std::fs::symlink_metadata(&base) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => true,
                    Ok(_) => {
                        failures.push(format!(
                            "cache path {} was not created as a real directory",
                            base.display()
                        ));
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!(
                            "cannot verify created cache {}: {error}",
                            base.display()
                        ));
                        continue;
                    }
                }
            }
            Err(error) => {
                failures.push(format!("cannot inspect {}: {error}", base.display()));
                continue;
            }
        };
        if created {
            if let Err(error) =
                std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
            {
                failures.push(format!(
                    "cannot restrict new cache {} to mode 0700: {error}",
                    base.display()
                ));
                continue;
            }
        }
        if !path_is_outside(&base, &boundary) {
            failures.push(format!(
                "created cache {} inside Cargo workspace {}",
                base.display(),
                boundary.display()
            ));
            continue;
        }
        return Ok(base);
    }
    Err(format!(
        "cannot create a cache outside the Cargo workspace: {}",
        failures.join("; ")
    ))
}

fn effective_cache_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".cache"))
        })
}

fn run_owned_cache_bracket() -> Result<(), String> {
    let fixture = tempfile::Builder::new()
        .prefix("validate-cache-self-test-")
        .tempdir()
        .map_err(|error| format!("run-owned cache: cannot create fixture: {error}"))?;
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir(&workspace)
        .map_err(|error| format!("run-owned cache: cannot create workspace: {error}"))?;
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = []\nresolver = \"2\"\n",
    )
    .map_err(|error| format!("run-owned cache: cannot write workspace manifest: {error}"))?;
    std::fs::write(workspace.join("probe.rs"), "fn main() {}\n")
        .map_err(|error| format!("run-owned cache: cannot write probe: {error}"))?;

    let generated_manifest = |cache: &Path, label: &str| -> Result<PathBuf, String> {
        let output = Command::new("rust-script")
            .args(["--package", "probe.rs"])
            .current_dir(&workspace)
            .env("XDG_CACHE_HOME", cache)
            .output()
            .map_err(|error| {
                format!("run-owned cache: cannot generate {label} probe package: {error}")
            })?;
        if !output.status.success() {
            return Err(format!(
                "run-owned cache: cannot generate {label} probe package: status={} stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let stdout = std::str::from_utf8(&output.stdout).map_err(|error| {
            format!("run-owned cache: {label} package path is not UTF-8: {error}")
        })?;
        let mut paths = stdout.lines().map(str::trim).filter(|line| !line.is_empty());
        let Some(package) = paths.next() else {
            return Err(format!(
                "run-owned cache: {label} package generation printed no path"
            ));
        };
        if paths.next().is_some() {
            return Err(format!(
                "run-owned cache: {label} package generation printed multiple paths: {stdout:?}"
            ));
        }
        let package = PathBuf::from(package);
        if package.as_os_str().is_empty() || !package.starts_with(cache) {
            return Err(format!(
                "run-owned cache: {label} probe package {} is not under cache {}",
                package.display(),
                cache.display()
            ));
        }
        let manifest = package.join("Cargo.toml");
        if !manifest.is_file() {
            return Err(format!(
                "run-owned cache: {label} probe package omitted {}",
                manifest.display()
            ));
        }
        Ok(manifest)
    };

    let cargo_metadata = |manifest: &Path| {
        Command::new("cargo")
            .args(["metadata", "--no-deps", "--format-version", "1", "--manifest-path"])
            .arg(manifest)
            .current_dir(&workspace)
            .output()
    };

    let inside = workspace.join("cache");
    let inside_manifest = generated_manifest(&inside, "inside-workspace")?;
    let failed = cargo_metadata(&inside_manifest)
        .map_err(|error| format!("run-owned cache: cannot inspect inside probe: {error}"))?;
    let failed_stderr = String::from_utf8_lossy(&failed.stderr);
    if failed.status.success()
        || !failed_stderr.contains("current package believes it's in a workspace when it's not")
    {
        return Err(format!(
            "run-owned cache: inside-workspace control did not reproduce Cargo's refusal: \
             status={} stderr={failed_stderr:?}",
            failed.status
        ));
    }

    let unsafe_parent = fixture.path().join("unsafe-parent");
    let unsafe_cache = unsafe_parent.join("ignored/validate/cache");
    std::fs::create_dir_all(&unsafe_cache)
        .map_err(|error| format!("run-owned cache: cannot create unsafe control: {error}"))?;
    std::fs::set_permissions(&unsafe_cache, std::fs::Permissions::from_mode(0o777))
        .map_err(|error| format!("run-owned cache: cannot chmod unsafe control: {error}"))?;

    let outside_path = create_safe_cache(&workspace, Some(&unsafe_parent))?;
    let unsafe_mode = std::fs::symlink_metadata(&unsafe_cache)
        .map_err(|error| format!("run-owned cache: cannot re-read unsafe control: {error}"))?
        .mode()
        & 0o777;
    if outside_path == unsafe_cache || unsafe_mode != 0o777 {
        return Err(format!(
            "run-owned cache: unsafe existing directory was accepted or mutated: selected={} \
             unsafe={} mode={unsafe_mode:04o}",
            outside_path.display(),
            unsafe_cache.display()
        ));
    }
    if !path_is_outside(&outside_path, &workspace) {
        return Err(format!(
            "run-owned cache: selected path {} is still inside {}",
            outside_path.display(),
            workspace.display()
        ));
    }
    let outside_manifest = generated_manifest(&outside_path, "outside-workspace")?;
    let passed = cargo_metadata(&outside_manifest)
        .map_err(|error| format!("run-owned cache: cannot inspect outside probe: {error}"))?;
    if !passed.status.success() {
        return Err(format!(
            "run-owned cache: outside-workspace probe failed: status={} stderr={:?}",
            passed.status,
            String::from_utf8_lossy(&passed.stderr)
        ));
    }
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    // Answered before anything else because it is a question ABOUT THE MACHINE,
    // not a validation run: no handlers, no log, no plan, no gate.
    if let Some(code) = probe_host_capability_query() {
        return ExitCode::from(code);
    }
    // This belongs to the one process admitted by ci-hub. Nested validator
    // invocations must not inherit authority to publish a competing result.
    let service_result_path = std::env::var_os(VALIDATE_SERVICE_RESULT_PATH_ENV).map(PathBuf::from);
    std::env::remove_var(VALIDATE_SERVICE_RESULT_PATH_ENV);
    install_stop_handlers();
    let started = std::time::Instant::now();

    // The durable log outlives `run` so the summary lands INSIDE it.
    let mut durable: Option<DurableLog> = None;
    let summary = run(&mut durable, service_result_path.as_deref());
    if let Err(error) = publish_validation_service_result(service_result_path.as_deref(), &summary) {
        eprintln!("validate: ERROR: {error}");
    }
    print_run_summary(&summary, started);
    if let Some(d) = durable.take() {
        d.finish();
    }
    ExitCode::from(summary.exit_code)
}

/// The whole invocation, returning what it concluded rather than an exit code.
const RUN_STATE_SCOPE_REEXEC_ENV: &str = "HERMIT_VALIDATE_RUN_STATE_SCOPE_REEXEC";

fn validation_run_state_path(
    root: &Path,
    profile: &str,
    output_path: Option<&Path>,
    inherited: Option<&OsStr>,
    nested: bool,
    pid: u32,
    nonce: u128,
) -> Result<PathBuf, String> {
    if nested {
        let path = inherited
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                "nested validation did not inherit VALIDATE_RUN_STATE from its outer run".to_string()
            })?;
        let run_root = root.join("target/validation");
        if !path.starts_with(&run_root) {
            return Err(format!(
                "nested validation inherited VALIDATE_RUN_STATE={} outside {}",
                path.display(),
                run_root.display()
            ));
        }
        return Ok(path);
    }
    if let Some(value) = inherited.filter(|value| !value.is_empty()) {
        return Err(format!(
            "top-level validation refuses inherited VALIDATE_RUN_STATE={}; each run owns a unique state directory",
            Path::new(value).display()
        ));
    }
    if let Some(parent) = output_path.and_then(Path::parent) {
        return Ok(parent.join("run-state"));
    }
    if profile.is_empty()
        || !profile
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(format!("validation profile is not path-safe: {profile:?}"));
    }
    Ok(root
        .join("target/validation")
        .join(format!("run-{profile}-{pid}-{nonce}")))
}

fn run_state_path_bracket() -> Result<(), String> {
    let root = Path::new("/repo");
    let profiles = ["full", "portable", "quick", "strict-compat-only"];
    let mut observed = BTreeSet::new();
    for (index, profile) in profiles.iter().enumerate() {
        let path = validation_run_state_path(
            root,
            profile,
            None,
            None,
            false,
            41,
            100 + index as u128,
        )?;
        if !path.starts_with(root.join("target/validation")) || path == Path::new("/") {
            return Err(format!(
                "{profile} chose a run-state path outside the run-owned root: {}",
                path.display()
            ));
        }
        observed.insert(path);
    }
    let first = validation_run_state_path(root, "full", None, None, false, 41, 200)?;
    let second = validation_run_state_path(root, "full", None, None, false, 41, 201)?;
    if first == second || observed.len() != profiles.len() {
        return Err("two direct validation runs reused a run-state path".into());
    }
    if validation_run_state_path(
        root,
        "full",
        None,
        Some(OsStr::new("/caller/chosen")),
        false,
        41,
        202,
    )
    .is_ok()
    {
        return Err("a top-level run accepted caller-owned VALIDATE_RUN_STATE".into());
    }
    let inherited = validation_run_state_path(
        root,
        "strict-compat-only",
        None,
        Some(OsStr::new("/repo/target/validation/outer")),
        true,
        41,
        203,
    )?;
    if inherited != Path::new("/repo/target/validation/outer") {
        return Err("a nested focused run did not preserve its outer run state".into());
    }
    if validation_run_state_path(
        root,
        "strict-compat-only",
        None,
        Some(OsStr::new("/caller/chosen")),
        true,
        41,
        204,
    )
    .is_ok()
    {
        return Err("a nested run accepted state outside the checkout-owned run root".into());
    }
    Ok(())
}

fn verified_run_state_scope_reexec(root: &Path, inherited: Option<&OsStr>) -> bool {
    let Some(inherited) = inherited
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    else {
        return false;
    };
    if std::env::var_os(RUN_STATE_SCOPE_REEXEC_ENV).as_deref() != Some(inherited.as_os_str())
        || !inherited.starts_with(root.join("target/validation"))
        || !is_in_scope()
    {
        return false;
    }
    let Ok(unit) = std::env::var("DAGRUN_SCOPE_UNIT") else {
        return false;
    };
    !unit.is_empty() && observe_own_containment(Some(&unit)).proof().is_some()
}

fn run(durable_slot: &mut Option<DurableLog>, service_result_path: Option<&Path>) -> RunSummary {
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

    if args.self_test && std::env::var_os(SUMMARY_EPILOGUE_SELF_TEST_ENV).is_some() {
        return RunSummary::refused(
            3,
            "self-test",
            "the per-checkout invocation lock",
            vec!["another validate is already running".into()],
        )
        .with_epilogue(vec![
            "watch the holder's live log with:".into(),
            "  tail -F -- $'/tmp/holder run.log'".into(),
        ]);
    }

    if nested_scope_probe_selected(args.self_test, nested_scope_probe_requested()) {
        return match run_nested_scope_probe() {
            Ok(detail) => RunSummary::new(
                Verdict::SelfTest, 0, "nested safe-ci scope self-test", vec![detail],
            ),
            Err(error) => {
                eprintln!("validate: NESTED SCOPE SELF-TEST FAILED: {error}");
                RunSummary::new(
                    Verdict::Fail, 2, "nested safe-ci scope self-test",
                    vec![format!("nested scope self-test failed: {error}")],
                )
            }
        };
    }

    if args.self_test {
        return match self_test() {
            Ok(()) => RunSummary::new(
                Verdict::SelfTest,
                0,
                "self-test",
                vec![
                    "force-full policy brackets, shell quoting, corpus counts, super gate table, \
                     envelope scoring/comparison, ledger cache, receipt eligibility, and the \
                     selective/only subset builders all passed"
                        .into(),
                    "policy/data brackets are inert; the cgroup bracket runs only inside a bounded \
                     disposable scope and neither publishes nor writes the real ledger"
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
    let discovered_parent = find_parent(&root);
    let parent = std::env::var_os(PARENT_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or(discovered_parent);
    if std::env::var_os(PARENT_ENV).is_none() {
        if let Some(parent) = &parent {
            // Child test-harness processes use the same parent checkout for the
            // per-cell series writer. The validate driver already discovered
            // this path for its ledger, so do not make every child rediscover it.
            std::env::set_var(PARENT_ENV, parent);
        }
    }
    let tool_root = match configured_tool_root(parent.as_deref()) {
        Ok(tool_root) => tool_root,
        Err(error) => {
            return RunSummary::refused(
                2,
                &level_name,
                "dev-hermit tool root",
                vec![
                    error,
                    format!(
                        "an explicit {TOOL_ROOT_ENV} must identify the immutable dev-hermit checkout whose ci-hub code launched this run"
                    ),
                ],
            );
        }
    };
    // The profile name is needed by the admission gates below, which run BEFORE
    // the plan exists. It is derived exactly as `build_plan` derives it, so the
    // lock record and the ledger row can never disagree about what was running.
    let profile_name =
        args.focused.as_ref().map(|f| f.profile()).unwrap_or_else(|| level_name.clone());

    // ---- re-entrancy (validate.sh:460) ---------------------------------------
    //
    // What must never happen is a full driver inside a full driver: it pays
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
    if nesting.nested && args.focused.is_none() && !args.show_plan {
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
        return stop_test_seam(
            &root,
            &profile_name,
            parent.as_deref(),
            tool_root.as_deref(),
            args.allow_local_off_the_record_run,
        );
    }

    if args.allow_local_off_the_record_run {
        if let Some(refusal) = local_off_the_record_refusal(&args, tree_dirty()) {
            eprintln!("{refusal}");
            return RunSummary::refused(
                2,
                &profile_name,
                "the local off-the-record run policy",
                refusal.lines().map(str::to_string).collect(),
            );
        }
        eprintln!(
            "validate: local iterative run is OFF THE RECORD: it may help find and fix a failure, \
             but it writes no ledger row, publishes no receipt, and cannot be cited as validation \
             evidence."
        );
    }

    // ---- dev-hermit product front door -------------------------------------
    //
    // Refuse real product work before the invocation lock, cache, cgroup boxing,
    // durable log, ledger, or DAG can create side effects. A parent `ci-hub/`
    // directory identifies dev-hermit; within that boundary, a missing or
    // unreadable launcher/authority is a refusal rather than a standalone
    // escape. Nested focused payloads are legitimate descendants of the same
    // lock owner and pass that canonical check; they are not exempted based on
    // their caller-supplied nesting marker.
    // Help, self-test and the stop-test seam returned above; `--show-plan` is
    // explicitly inert here.
    let ci_hub_dir_present =
        tool_root.as_ref().is_some_and(|candidate| candidate.join("ci-hub").is_dir());
    if !args.allow_local_off_the_record_run
        && product_front_door_applies(
        parent.is_some(),
        ci_hub_dir_present,
        nesting.nested,
        args.show_plan,
    )
    {
        let tool_root = tool_root
            .as_deref()
            .expect("front-door predicate requires an executable tool root");
        let commit = git_sha();
        let host = short_hostname();
        let ci_hub_launcher_available = tool_root.join("ci-hub/ci-hub").is_file();
        let admission = validate_lock_admission(Some(tool_root), &commit, &host);
        // NAME THE CONJUNCT THAT FAILED. The decision is unchanged -- it is still
        // exactly `admission.is_ok()` -- but a refusal that lists three
        // possibilities and identifies none is undiagnosable from outside, and
        // that is what left the owner unable to see that his checkout simply was
        // not at the commit his lock was taken for.
        let admitted = admission.is_ok();
        let why = admission.err();
        if let Some(refusal) = product_front_door_refusal(
            tool_root,
            &root,
            &commit,
            &requested_validate_args(),
            ci_hub_launcher_available,
            admitted,
        ) {
            eprintln!("{refusal}");
            if let Some(reason) = &why {
                eprintln!("\nWhy this run was not admitted:\n  {reason}");
            }
            let mut detail = vec![match &why {
                Some(reason) => format!(
                    "the dev-hermit boundary was detected and admission was not established: \
                     {reason}"
                ),
                None => "the dev-hermit boundary was detected, but exact-commit, exact-host, \
                         live validate-lock owner ancestry was not established"
                    .to_string(),
            }];
            detail.push(
                "repair ci-hub if needed, then use its validate-run entry point; environment \
                 markers cannot authorize product work"
                    .into(),
            );
            return RunSummary::refused(
                4,
                &profile_name,
                "the dev-hermit product front door",
                detail,
            );
        }
    }

    if let Err(error) = establish_cell_host_facts(nesting.nested) {
        return RunSummary::refused(
            2,
            &profile_name,
            "cell-result host facts",
            vec![error],
        );
    }

    // Anchor the logical run before locks, freshness checks, plan construction, cgroup re-exec,
    // durable-log setup, and registration.  A nested focused payload inherits the enclosing
    // safe-ci step's scheduler-owned epoch; a top-level run owns its epoch here.
    let run_timeout = effective_run_timeout(
        args.run_timeout,
        env_positive("HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS"),
        args.show_plan,
    );
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
    let mut invocation_lock;
    if !nesting.nested && !args.show_plan {
        match validate_runtime::acquire_invocation_lock(&root, &profile_name, &git_sha()) {
            validate_runtime::LockOutcome::Acquired(l) => invocation_lock = Some(l),
            validate_runtime::LockOutcome::Busy { detail, epilogue } => {
                return RunSummary::refused(
                    3,
                    &profile_name,
                    "the per-checkout invocation lock",
                    detail,
                )
                .with_epilogue(epilogue);
            }
            validate_runtime::LockOutcome::SafetyRefusal(error) => {
                return RunSummary::refused(
                    3,
                    &profile_name,
                    "the per-checkout invocation safety guard",
                    vec![error],
                );
            }
            validate_runtime::LockOutcome::Unavailable(e) => {
                return unavailable_invocation_lock_summary(&profile_name, e);
            }
        }
    } else {
        invocation_lock = None;
    }

    // Dirty-tree gate, BEFORE any state is created, so a refusal leaves nothing
    // behind. A result validated against uncommitted changes describes a tree
    // that exists nowhere in history and cannot be reproduced or compared.
    // Skipped for a nested payload: the outer run already made this judgement
    // about the same checkout, and a second answer could only disagree.
    let dirty_at_admission = tree_dirty();
    let wt_dirty = worktree_dirty();
    if args.write_constructed_dag.is_none()
        && args.write_generated_plan.is_none()
        && dirty_worktree_requires_refusal(
            nesting.nested,
            wt_dirty,
            args.skip_inner_dirty_working_tree_and_rebase_freshness_checks,
        )
    {
        eprintln!("validate: refusing to run on a dirty working tree.");
        eprintln!("  HEAD {} has uncommitted working-tree changes, so a record anchored to it", git_sha());
        eprintln!("  would describe a tree that exists nowhere in history. Commit (preferred), or");
        eprintln!("  stage the WIP with 'git add', then re-run. To force an explicitly unanchored");
        eprintln!(
            "  run pass --skip-inner-dirty-working-tree-and-rebase-freshness-checks \
             (agents must not). This skips only scripts/validate.rs's dirty-working-tree and"
        );
        eprintln!("  rebase-freshness checks; it does not bypass ci-hub validate-lock admission.");
        let _ = Command::new("git").args(["status", "--short"]).status();
        return RunSummary::refused(
            2,
            &profile_name,
            "the dirty-working-tree gate",
            vec![
                "HEAD has uncommitted working-tree changes, so a record anchored to it would \
                 describe a tree that exists nowhere in history"
                    .into(),
                "commit (preferred) or `git add` the WIP, then re-run; \
                 --skip-inner-dirty-working-tree-and-rebase-freshness-checks forces an explicitly \
                 unanchored run but does not bypass ci-hub validate-lock admission"
                    .into(),
            ],
        );
    }

    // Rebase-freshness gate. Mechanically enforced, not advisory. A nested
    // payload inherits the outer run's verdict on the very same checkout; it also
    // must not spend a network round trip inside a budgeted DAG node.
    match rebase_freshness(
        args.skip_inner_dirty_working_tree_and_rebase_freshness_checks
            || nesting.nested
            || args.write_constructed_dag.is_some()
            || args.write_generated_plan.is_some(),
    ) {
        Ok(msg) => eprintln!("validate: {msg}"),
        Err(msg) => {
            eprintln!("validate: refusing to validate a stale base.\n  {msg}");
            return RunSummary::refused(
                2,
                &profile_name,
                "the rebase-freshness gate",
                msg.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
            );
        }
    }

    // rust-script asks Cargo to build a generated package under XDG_CACHE_HOME.
    // If that cache is anywhere below this checkout (or an enclosing Cargo
    // workspace), Cargo refuses the generated package as an undeclared member.
    // Keep one stable cache outside every Cargo-manifest ancestor so focused
    // runs remain warm; nested payloads inherit it.
    if !nesting.nested && !args.show_plan {
        let boundary = cargo_manifest_boundary(&root);
        let effective_cache = effective_cache_path();
        if effective_cache
            .as_deref()
            .is_none_or(|path| !path_is_outside(path, &boundary))
        {
            if let Some(path) = effective_cache {
                eprintln!(
                    "validate: effective cache path {} is inside Cargo workspace {}; using a \
                     shared cache outside it",
                    path.display(),
                    boundary.display()
                );
            }
            let cache = match create_safe_cache(&root, parent.as_deref()) {
                Ok(cache) => cache,
                Err(error) => {
                    return RunSummary::refused(
                        2,
                        &profile_name,
                        "run-owned cache setup",
                        vec![error],
                    )
                }
            };
            std::env::set_var("XDG_CACHE_HOME", cache);
        }
    }

    // Run state lives under target/, never under HERMIT_DIR (a user setting).
    // A constructed DAG exported for ci/run-dag.sh keeps its run-owned paths
    // beside the exported file so the caller can remove the entire temporary
    // directory after the one scheduler exits.
    let output_path = args
        .write_constructed_dag
        .as_deref()
        .or(args.write_generated_plan.as_deref());
    let run_state_nonce = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(error) => {
            return RunSummary::refused(
                2,
                &profile_name,
                "run-state setup",
                vec![format!("clock is before the Unix epoch: {error}")],
            )
        }
    };
    let inherited_run_state = std::env::var_os("VALIDATE_RUN_STATE");
    let verified_scope_reexec =
        verified_run_state_scope_reexec(&root, inherited_run_state.as_deref());
    std::env::remove_var(RUN_STATE_SCOPE_REEXEC_ENV);
    if verified_scope_reexec {
        eprintln!("validate: preserving its run-owned state across the verified cgroup scope re-exec");
    }
    let tmp = match validation_run_state_path(
        &root,
        &profile_name,
        output_path,
        inherited_run_state.as_deref(),
        nesting.nested || verified_scope_reexec,
        std::process::id(),
        run_state_nonce,
    ) {
        Ok(path) => path,
        Err(error) => {
            return RunSummary::refused(2, &profile_name, "run-state setup", vec![error])
        }
    };
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return RunSummary::refused(
            2,
            &profile_name,
            "run-state setup",
            vec![format!("cannot create {}: {e}", tmp.display())],
        );
    }
    std::env::set_var("VALIDATE_RUN_STATE", &tmp);
    if !nesting.nested {
        for (variable, name) in [
            ("TMPDIR", "tmp"),
            ("PYTHONPYCACHEPREFIX", "python-cache"),
            ("HERMIT_DATA_DIR", "hermit-data"),
        ] {
            if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
                continue;
            }
            let path = tmp.join(name);
            if let Err(error) = std::fs::create_dir_all(&path) {
                return RunSummary::refused(
                    2,
                    &profile_name,
                    "run-owned temporary path setup",
                    vec![format!("cannot create {} for {variable}: {error}", path.display())],
                );
            }
            std::env::set_var(variable, path);
        }
    }

    let mut plan = match if args.write_generated_plan.is_some() {
        build_generated_validation_plan(&root, &tmp)
    } else {
        build_plan(&root, &args, &tmp)
    } {
        Ok(p) => p,
        Err(e) => {
            eprintln!("validate: cannot build the execution plan: {e}");
            return RunSummary::refused(
                2,
                &profile_name,
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

    if plan.committed_selection.is_none() {
        if let Err(error) =
            configure_prebuilt_rust_scripts(&root, &mut plan, args.ignore_selected_deps)
        {
            return RunSummary::refused(
                2,
                &plan.profile,
                "rust-script build-plan construction",
                vec![error],
            );
        }
    }

    if plan.committed_selection.is_none() {
        assign_fail_fast_families(&mut plan);
        propagate_verbosity(&mut plan, args.verbosity);
    } else {
        // Verbosity is execution policy, not graph topology. Children inherit
        // this value without validate rewriting every committed step's env.
        std::env::set_var("VALIDATE_VERBOSITY", args.verbosity.to_string());
    }

    // A node this machine provably cannot run is withheld here, BEFORE anything
    // spawns, and recorded as host-inapplicable. Nothing a node DOES can reach
    // this decision, so a node that is merely broken still runs and still fails.
    let capability_result = if plan.committed_selection.is_some() {
        require_host_capabilities(&root, &plan)
    } else {
        withhold_host_inapplicable(&root, &mut plan)
    };
    if let Err(e) = capability_result {
        eprintln!("validate: cannot resolve host-capability requirements: {e}");
        return RunSummary::refused(
            2,
            &level_name,
            "host-capability resolution",
            vec![
                e,
                "no node was omitted and no substitute profile was run: an unevaluable capability \
                 declaration is refused, never treated as a reason to skip work"
                    .into(),
            ],
        );
    }

    // Per-gate budget overrides, preserved from validate.sh
    // (VALIDATE_GATE_TIMEOUT_SECONDS / VALIDATE_GATE_CPU_TIMEOUT_SECONDS). These
    // LOWER a node's ceiling, never raise it: a caller tightening budgets to
    // reproduce a timeout must not accidentally loosen a node that already
    // declared something stricter. They are also how the timeout path is
    // exercised on demand without waiting for a real runaway.
    // DERIVE every node's ceiling from the budget that will actually be ENFORCED.
    // The scheduler is handed `remaining_budget_s(deadline)`, not the nominal run
    // budget, so a ceiling chosen BESIDE the nominal one silently inverts as soon
    // as preparation has spent part of the epoch: the node budget stops being
    // smaller than the bound that will cut it, and the scheduler refuses the whole
    // lane rather than running work it could not attribute.
    //
    // Measured 2026-08-25 on the strict-compat lane: the ladder is written
    // `420 prep < 480 gate < 600 run`, but prep and gate are spent SEQUENTIALLY
    // from one clock, so the run budget would have to be at least 900s for that to
    // hold. 159s of the 600s epoch was already gone when the scheduler started,
    // leaving 441s against a 480s gate ceiling, and all 193 compat nodes reported
    // nothing. Deriving the ceiling here makes the inversion unreachable instead of
    // making it fit for one particular preparation time -- the same fixed-versus-
    // derived defect as a node pinned at 120s losing to a 120.03s measurement.
    if plan.committed_selection.is_none() {
        if let Some(remaining) = remaining_budget_s(deadline_ns) {
            clamp_wall(&mut plan, derived_wall_ceiling(remaining));
        }
        if let Some(cap) = env_positive("VALIDATE_GATE_TIMEOUT_SECONDS") {
            clamp_wall(&mut plan, cap);
            eprintln!("validate: VALIDATE_GATE_TIMEOUT_SECONDS={cap}: every gate's wall ceiling lowered to at most {cap}s");
        }
        if let Some(cap) = env_positive("VALIDATE_GATE_CPU_TIMEOUT_SECONDS") {
            clamp_cpu(&mut plan, cap);
            eprintln!("validate: VALIDATE_GATE_CPU_TIMEOUT_SECONDS={cap}: every gate's CPU budget lowered to at most {cap}s");
        }
    } else if [
        "VALIDATE_GATE_TIMEOUT_SECONDS",
        "VALIDATE_GATE_CPU_TIMEOUT_SECONDS",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
    {
        return RunSummary::refused(
            2,
            &plan.profile,
            "committed-DAG timeout policy",
            vec![
                "VALIDATE_GATE_TIMEOUT_SECONDS and VALIDATE_GATE_CPU_TIMEOUT_SECONDS would rewrite committed node budgets; edit/regenerate ci/dag/validate.json or use dagrun's CPU-timeout multiplier"
                    .into(),
            ],
        );
    }

    if let Err(error) = require_committed_scheduler_input(&plan) {
        return RunSummary::refused(
            2,
            &plan.profile,
            "committed-DAG scheduler boundary",
            vec![error],
        );
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
            .chain(capped_refusal_items(
                ungrantable.iter().map(|b| format!("  {b}")).collect(),
            ))
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
    // Refuse an inverted ladder for every execution. A node with an allowance
    // at least as large as the run budget can only be cut by the less-specific
    // outer clock, losing attribution to the node. `--show-plan` has no run
    // deadline and executes nothing, so an inherited execution budget is not
    // applicable to its raw, pre-wrapping plan. An explicit `--run-timeout`
    // still asks to audit that prospective ladder.
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
                .chain(capped_refusal_items(
                    bad.iter()
                        .map(|(tag, t)| format!("  {tag} ({t}s)"))
                        .collect(),
                ))
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
    if let Some(path) = args
        .write_constructed_dag
        .as_ref()
        .or(args.write_generated_plan.as_ref())
    {
        if plan.second.is_some() {
            return RunSummary::refused(
                2,
                &plan.profile,
                "constructed DAG export",
                vec![
                    "--write-constructed-dag requires one constructed DAG; use a merged profile or one lane"
                        .into(),
                ],
            );
        }
        let mut exported = plan.cfg.clone();
        if args.write_generated_plan.is_some() {
            if let Err(error) = materialize_source_cpu_timeouts(&mut exported) {
                return RunSummary::refused(
                    2,
                    &plan.profile,
                    "source DAG export",
                    vec![error],
                );
            }
        }
        if let Err(error) = std::fs::write(path, format!("{}\n", dag_to_json(&exported))) {
            return RunSummary::refused(
                2,
                &plan.profile,
                "constructed DAG export",
                vec![format!("cannot write {}: {error}", path.display())],
            );
        }
        return RunSummary::new(
            Verdict::PlanOnly,
            0,
            &plan.profile,
            vec![
                format!("complete constructed outer DAG written to {}", path.display()),
                "nothing was executed and no ledger row was written".into(),
            ],
        );
    }
    if args.show_plan {
        let mut all: Vec<&DagConfig> = vec![&plan.cfg];
        if let Some(s) = &plan.second {
            all.push(s);
        }
        if args.show_plan_json {
            let dags = all
                .iter()
                .map(|cfg| {
                    serde_json::json!({
                        "description": cfg.description,
                        "steps": cfg.steps.iter().map(|step| serde_json::json!({
                            "tag": step.tag(),
                            "deps": step.deps,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "profile": plan.profile,
                    "selection_mode": plan.selection_mode,
                    "dags": dags,
                }))
                .expect("constructed plan is serializable")
            );
            return RunSummary::new(Verdict::PlanOnly, 0, &plan.profile, vec![
                "--show-plan-json: constructed outer steps printed".into(),
                "nothing was executed and no ledger row was written".into(),
            ]);
        }
        println!("profile: {}  selection: {}", plan.profile, plan.selection_mode);
        for (i, cfg) in all.iter().enumerate() {
            println!("\n--- DAG {} of {} ({}) : {} node(s)", i + 1, all.len(), cfg.description, cfg.steps.len());
            println!("{:<40} {:>7} {:>7} {:>8}  deps", "node", "wall_s", "cpu_s", "mem");
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
        println!(
            "\ntotal outer boxed nodes: {total}; all have declared wall+cpu+memory caps (audited above)."
        );
        println!(
            "This output does not enumerate Rust test IDs or E2E cells inside those outer nodes."
        );
        return RunSummary::new(
            Verdict::PlanOnly,
            0,
            &plan.profile,
            vec![
                format!("--show-plan: {total} outer boxed node(s) printed, all with declared wall+cpu+memory caps"),
                "Rust test IDs and E2E cells inside those nodes were not enumerated".into(),
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
        && !args.allow_local_off_the_record_run
        && !args.ignore_cache
        && plan.cacheable
        && !wt_dirty
        && !dirty_at_admission
        && plan.selection_mode == "full"
    {
        if let Some(hit) = validate_history::cache_lookup(&ledger_rows, "pass", &cache_key) {
            match cache_hit_run_summary(
                &hit,
                &plan.profile,
                plan.selection_mode,
                &tree,
                &ledger,
                service_result_path.is_some(),
            ) {
                Ok(summary) => {
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
                    println!(
                        "#   profile={} host={host} toolchain={toolchain}",
                        plan.profile
                    );
                    println!(
                        "#   NO gates ran this invocation; reused a clean, commit-anchored passing"
                    );
                    println!(
                        "#   record (nonzero executed count, satisfied gate coverage) from the"
                    );
                    println!("#   run-ledger ({}).", ledger.display());
                    println!("# ============================================================");
                    let _ = std::fs::remove_dir_all(&tmp);
                    return summary;
                }
                Err(reason) => eprintln!(
                    "# validate: {reason}; ignoring this cache row and running validation"
                ),
            }
        }
        // A prior genuine FAIL prevents the PASS cache lookup above from
        // succeeding. Note it and run so targeted requalification evidence can
        // be produced; a lucky sibling PASS cannot turn this invocation into a
        // zero-gate cache hit.
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

    std::env::set_var(RUN_STATE_SCOPE_REEXEC_ENV, &tmp);
    let cgroup_result = resolve_cgroups(
            args.allow_cgroup_failure,
            run_timeout,
            deadline_ns,
            service_result_path,
        );
    std::env::remove_var(RUN_STATE_SCOPE_REEXEC_ENV);
    let cgroups: BoxedCgroups =
        match cgroup_result {
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
    let git_depth = match measure_git_depth(&commit) {
        Ok(depth) => depth,
        Err(error) => {
            return RunSummary::refused(
                2,
                &plan.profile,
                "git depth measurement",
                vec![
                    error,
                    "the schema requires a real git_depth; refusing instead of omitting it or inventing a value"
                        .into(),
                ],
            )
        }
    };
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
    // Now that the log exists, tell the holder record where it is, so a validate
    // REFUSED against this one can print a command to tail it. Gated on actually
    // holding the lock: a nested payload must never rewrite the outer run's
    // record, and an UNGUARDED run (lock unavailable) has no record to append to.
    if let Some(lock) = invocation_lock.as_mut() {
        validate_runtime::record_invocation_log_path(lock, &log_path);
    }
    let e2e_result_root =
        match configure_e2e_result_root(&root, &log_path, &tmp.join("e2e-build")) {
            Ok(path) => path,
            Err(message) => {
                eprintln!("validate: ERROR: {message}");
                return RunSummary::refused(
                    4,
                    &plan.profile,
                    "durable per-cell result setup",
                    vec![
                        message,
                        "a validate without retained per-cell rows cannot produce the compatibility table"
                            .into(),
                    ],
                );
            }
        };
    eprintln!("validate: per-cell results: {}", e2e_result_root.display());

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
        match validate_runtime::register_run(&registry, &plan.profile, &root) {
            Ok(record) => Some(record),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&tmp);
                let mut summary = RunSummary::refused(
                    COULD_NOT_RUN_EXIT_CODE,
                    &plan.profile,
                    "box-wide live-run registration",
                    vec![
                        error,
                        "concurrency accounting is required evidence; refusing rather than \
                         running unregistered and reporting zero peers"
                            .into(),
                    ],
                );
                summary.log = Some(log_path.clone());
                return summary;
            }
        }
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
        match nesting.outer_pid {
            Some(outer) => println!(
                "Nested validate (payload of outer pid {outer}): focused mode {} only; the outer run owns \
                 the ledger, receipt, cache, invocation lock and concurrency accounting.",
                plan.profile
            ),
            None => println!(
                "Nested validate (exact pinned-root payload): focused mode {} only; the outer run owns \
                 the ledger, receipt, cache, invocation lock and concurrency accounting.",
                plan.profile
            ),
        }
    }

    let jobs = args.jobs.unwrap_or_else(default_jobs);
    let started_at = utc_now();
    let started_epoch = epoch_now();
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let node_count = plan.cfg.steps.len() + plan.second.as_ref().map(|c| c.steps.len()).unwrap_or(0);
    // Every node the profile PLANNED, including the ones withheld as
    // host-inapplicable. The ledger's `unaccounted_nodes` is computed against
    // this set, so a node that neither ran nor carries a recorded reason is
    // named rather than lost.
    let planned_tags: BTreeSet<String> = std::iter::once(&plan.cfg)
        .chain(plan.second.iter())
        .flat_map(|cfg| cfg.steps.iter().map(|s| s.tag()))
        .chain(plan.host_inapplicable.iter().map(|n| n.tag.clone()))
        .collect();

    println!("Validation profile: {} (selection: {})", plan.profile, plan.selection_mode);
    println!(
        "Commit: {commit} ({})",
        if dirty_at_admission {
            "⚠️  NOT commit-anchored: dirty tree at admission"
        } else {
            "clean tree at admission; commit anchoring finalizes after the run"
        }
    );
    println!("Build cache: {cache}; host cores: {host_cpus}; scheduler width: -j {jobs}");
    println!(
        "Plan: {node_count} boxed DAG node(s){}{}",
        if plan.second.is_some() { " across 2 sequential lanes" } else { "" },
        if plan.host_inapplicable.is_empty() {
            String::new()
        } else {
            format!(
                "; {} planned node(s) withheld as host-inapplicable and NOT counted as passing: {}",
                plan.host_inapplicable.len(),
                plan.host_inapplicable
                    .iter()
                    .map(|n| n.tag.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
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
    let mut attempts: Vec<NodeAttempt> = Vec::new();
    let mut ok = true;
    let mut execution_complete = true;

    // One clock for the whole invocation. Sequential lanes spend from the same
    // allowance rather than each receiving a fresh budget.
    let deadline = deadline_ns;
    let lane = |cfg: &DagConfig| -> LaneResult {
        run_lane_once(
            cfg,
            jobs,
            keep_going,
            verbosity,
            cgroups.clone(),
            &log_path,
            deadline,
            true,
        )
    };
    let mut run_timed_out = false;

    let r = lane(&plan.cfg);
    outcomes.extend(r.outcomes.iter().cloned());
    skipped.extend(r.skipped.iter().cloned());
    attempts.extend(r.attempts.iter().cloned());
    ok = ok && r.ok;
    execution_complete = execution_complete && r.complete;
    run_timed_out = run_timed_out || r.run_timed_out;

    if let Some(second) = &plan.second {
        // Sequential lanes are separate fail-fast families. A failure in the first lane must not
        // suppress the second: the lane runner still cancels failed-family peers and skips true
        // dependents, while the second lane records its own real outcomes.
        let r2 = lane(second);
        outcomes.extend(r2.outcomes.iter().cloned());
        skipped.extend(r2.skipped.iter().cloned());
        attempts.extend(r2.attempts.iter().cloned());
        ok = ok && r2.ok;
        execution_complete = execution_complete && r2.complete;
        run_timed_out = run_timed_out || r2.run_timed_out;
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
    let classification = classify_run(
        &outcomes, &attempts, &skipped, &planned_tags, &plan.host_inapplicable,
    );
    let validation_complete = validation_is_complete(
        execution_complete, &classification, &planned_tags,
    );
    print_cost_table(&outcomes, &attempts, &skipped, &plan.host_inapplicable);
    print_retry_ledger(&attempts);

    // ---- the single cleanup / evidence-commit point (validate.sh:1812) -------
    //
    // From here to the ledger append is ONE critical section. A second stop
    // signal must not abort it between teardown and the append, or a run that did
    // real work would leave no record of having run at all — which reads exactly
    // like never having started. `SIG_IGN` for the window is what `trap ''
    // INT TERM HUP` bought the bash.
    validate_runtime::enter_cleanup_critical_section();
    let interruption = interrupted_by().map(|s| s.to_string());
    let series_error = if nesting.nested || args.allow_local_off_the_record_run {
        None
    } else {
        match append_validate_series(
            parent.as_deref(),
            tool_root.as_deref(),
            &root,
            &e2e_result_root,
            &commit,
        ) {
            Ok(_) => None,
            Err(error) => {
                eprintln!("validate: ERROR: completed cell results were not added to the series: {error}");
                Some(error)
            }
        }
    };
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
    let (executed_tests, passed_tests, filtered_tests, compatibility_count_error) =
        match run_test_counts(
            &outcomes,
            &attempts,
            plan.compat,
            plan.compat_prefix,
        ) {
            Ok((executed, passed, filtered)) => (executed, passed, filtered, None),
            Err(error) => {
                eprintln!("validate: ERROR: {error}");
                let (executed, passed, filtered) = libtest_counts(&outcomes);
                (executed, passed, filtered, Some(error))
            }
        };
    if executed_tests.is_none() {
        eprintln!(
            "validate: WARNING: libtest counts are UNKNOWN for this run. A ledger row with \
             executed_tests=null is a NON-VERDICT, not a green: no downstream completeness \
             predicate can qualify it."
        );
    }

    // The parent still supplies exact base and pin evidence, but its historical
    // per-node coverage parser reads printable banners. Rebuild coverage from
    // dagrun's structured producer results so stdout cannot qualify a node.
    let receipt = receipt_evidence(tool_root.as_deref(), &root, &log_path, &commit);
    let coverage = typed_test_node_coverage(&plan.planned_test_nodes, &outcomes);

    let behind_ahead = sh("git", &["rev-list", "--left-right", "--count", "origin/main...HEAD"])
        .unwrap_or_else(|| "0 0".into());
    let mut ba = behind_ahead.split_whitespace();
    let git_behind: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let git_ahead: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    // Observed, not inferred: did the pin gate actually run and pass in THIS run?
    let pin_gate_passed = outcomes.iter().any(|o| o.tag == PIN_GATE_TAG && o.ok);
    let checkout_admission = checkout_admission(
        &root,
        parent.as_deref(),
        tool_root.as_deref(),
        &commit,
        &host,
    );
    let lock_admitted = checkout_admission.lock_admitted;
    let dirty_after_run = tree_dirty();
    let admitted_disposable_checkout = checkout_admission.disposable;
    let attribution = source_attribution(
        &commit,
        dirty_at_admission,
        dirty_after_run,
        admitted_disposable_checkout,
    );
    let commit_anchored = attribution.commit_anchored;
    let attribution_tree_dirty = attribution.tree_dirty;
    if admitted_disposable_checkout && !dirty_at_admission && dirty_after_run {
        eprintln!(
            "validate: disposable checkout became dirty during execution; attribution remains \
             bound to the exact clean commit admitted before execution. End-of-run checkout \
             dirtiness is diagnostic and no pathname was exempted."
        );
    }
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
        git_depth,
        git_ahead,
        git_behind,
        commit_anchored,
        tree_dirty: attribution_tree_dirty,
        dag_jobs: jobs,
        admission: lock_admitted.then_some("ci-hub-validate-lock"),
        base_sha: receipt.base_sha,
        base_tree: receipt.base_tree,
        reverie_base_sha: receipt.reverie_base_sha,
        reverie_base_tree: receipt.reverie_base_tree,
        reverie_pin_current: pin_gate_passed,
        concurrent_validates: peak_active,
        concurrency_proof: if lock_admitted {
            Some(if peak_active.unwrap_or(0) == 0 {
                "validate_lock_owner_ancestry"
            } else {
                "validate_lock_owner_ancestry+live_flock_registry_cpu_delta"
            })
        } else {
            peak_active.map(|_| "live_flock_registry_cpu_delta")
        },
        interruption: interruption.clone(),
        cpu_user,
        cpu_sys,
        // Kept in the ledger schema for historical rows. Validate no longer
        // retries a whole outer DAG, so new rows always write zero here.
        retry_rounds: 0,
        executed_tests,
        passed_tests,
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
    // no_result verdict. Collected node limits remain failed conditions; a
    // whole-run deadline is incomplete and falls through to the normal fold below.
    if let Some(sig) = &interruption {
        if !nesting.nested && !args.allow_local_off_the_record_run {
            write_ledger(
                &ledger,
                &ctx,
                &outcomes,
                &attempts,
                &skipped,
                &plan.host_inapplicable,
                &planned_tags,
                wall,
                130,
                &log_path.to_string_lossy(),
                false,
                coverage.clone(),
                None,
            );
        }
        // This is below the interrupted run's ledger write. Keep the checkout
        // lock held while the generated files are replaced, so a second local
        // validate cannot begin against the tree between those two operations.
        let scorecard_writeback = local_scorecard_writeback(
            &root,
            &e2e_result_root,
            nesting.nested,
            args.allow_local_off_the_record_run,
        );
        drop(run_record);
        let _ = std::fs::remove_dir_all(&tmp);
        let mut detail = vec![
            format!("stopped by SIG{sig}; prior measured product failures remain failures"),
            validation_completeness_detail(false, classification.product_result_nodes.len(), planned_tags.len()),
        ];
        if let Some(error) = &series_error {
            detail.push(format!(
                "completed cell results could not be added to the series: {error}"
            ));
        }
        let mut s = RunSummary::new(
            Verdict::Interrupted,
            130,
            &plan.profile,
            detail,
        );
        s.nodes_executed = completed_node_count(&outcomes, &attempts);
        s.nodes_failed = classification.product_failure_nodes.len();
        s.nodes_skipped = skipped.len();
        s.nodes_host_inapplicable = plan.host_inapplicable.len();
        s.executed_tests = executed_tests;
        s.passed_tests = passed_tests;
        s.selection_mode = Some(plan.selection_mode.into());
        s.wall_s = Some(wall);
        s.jobs = Some(jobs);
        s.log = Some(log_path);
        s.cpu_wall = Some((wall, cpu_user, cpu_sys));
        if !nesting.nested && !args.allow_local_off_the_record_run {
            s.ledger = Some(ledger);
        }
        record_scorecard_writeback(&mut s, scorecard_writeback);
        return s;
    }

    // Compatibility ratchet, evaluated from typed outcomes.
    let mut compat_blocking = 0usize;
    let mut compat_nonblocking = BTreeSet::new();
    // Carried to the verdict: a compat profile that measured nothing must not be
    // able to reach PASS through an empty set of failing rows.
    let mut compat_measured: Option<usize> = None;
    if let Some(mode) = plan.compat {
        let prefix = plan
            .compat_prefix
            .expect("compatibility plans carry their committed tag prefix");
        let (passed, measured, blocking, nonblocking) =
            print_compat_summary(mode, prefix, &outcomes, &attempts);
        compat_blocking = blocking.len();
        compat_nonblocking = nonblocking;
        compat_measured = Some(measured);
        let floor = match mode {
            CompatMode::Sabre => Some(validate_corpus::SABRE_COMPAT_EXPECTED),
            CompatMode::Rr => Some(validate_corpus::RR_COMPAT_EXPECTED),
            CompatMode::Strict | CompatMode::PortableStrict | CompatMode::E9patch => None,
        };
        if let Some(f) = floor {
            if passed < f {
                println!("❌ {} ratchet: {passed}/{measured} passing, floor {f} — BELOW FLOOR", mode.display_name());
                ok = false;
            } else {
                println!("✅ {} ratchet: {passed}/{measured} passing, floor {f} — met", mode.display_name());
            }
        }
        if !blocking.is_empty() {
            println!("❌ {} blocking failures ({}): {}", mode.display_name(), blocking.len(), blocking.join(", "));
        }
    }

    // Super stress pass rates, from typed outcomes rather than a scraped report.
    if plan.super_mode {
        let reps = validate_super::repetitions();
        let rates = validate_classification::stress_rates(&classification, reps);
        // This is a per-PROBE display summary. The failed repetition nodes are
        // already in `blocking_failures`, so the grouped count must not be added
        // to the final node count.
        print_super_stress_verdict(&rates, reps, jobs, host_cpus);
    }

    // Working-envelope vector: score, emit JSON, print the human summary, and
    // enforce monotonicity when a baseline was supplied.
    let mut envelope_regressed = false;
    let mut envelope_error: Option<(u8, String)> = None;
    if let Some(env) = &plan.envelope {
        let short = sh("git", &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let vector = validate_envelope::score_with_passed(env.reps, &short, |tag| {
            classification.product_result_nodes.contains(tag)
                && !classification.product_failure_nodes.contains(tag)
        });
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

    let failures = classification.product_failure_nodes.len();
    let no_results = classification.no_results();
    let no_result_nodes: BTreeSet<&str> = classification.no_result_nodes.iter()
        .chain(classification.understood_infrastructure_failure_nodes.keys())
        .chain(classification.understood_prerequisite_failure_nodes.iter())
        .map(String::as_str).collect();
    let no_result_nodes: Vec<&str> = no_result_nodes.into_iter().collect();
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
    let blocking_failures = classification.blocking_failures(&plan.nonblocking);
    // Failures OUTSIDE the measured matrix: the build/prep/gate spine. `compat.*`
    // rows are excluded because the compat ratchet already judges them (and
    // excuses the known-fail-closed ones), so counting them here would both
    // double-count and re-block rows policy has excused. Everything else — a
    // failed `compatprep.*`, `pre.*`, `gate.*`, `build.*` — is a node whose
    // failure can EMPTY the matrix, and no matrix ratchet can speak to that.
    let structural_failures = classification.structural_failures(&plan.nonblocking);
    let effective_failures = effective_failure_count(
        plan.compat,
        blocking_failures,
        compat_blocking,
        structural_failures,
    );
    let summary_nonblocking: BTreeSet<String> = plan
        .nonblocking
        .union(&compat_nonblocking)
        .cloned()
        .collect();
    // `ok` from the runner reflects every node, including the nonblocking ones,
    // so it is only authoritative when nothing is excused. A known exit 75
    // fully explains why the runner returned non-ok; any other unexplained
    // non-ok state remains a failure.
    let unexplained_runner_failure =
        plan.nonblocking.is_empty() && plan.compat.is_none() && !ok && no_results == 0
            && failures == 0 && !run_timed_out && !outcomes.iter().any(outcome_is_failure);
    let mut exit_code = completed_exit_code(
        effective_failures,
        no_results,
        run_timed_out,
        unexplained_runner_failure,
    );
    if envelope_regressed {
        exit_code = 1;
    }
    if let Some((code, _)) = &envelope_error {
        exit_code = *code;
    }
    if series_error.is_some() {
        exit_code = 1;
    }
    if compatibility_count_error.is_some() {
        exit_code = 1;
    }
    if !execution_complete {
        eprintln!(
            "validate: ERROR: not every required node completed with a non-aborted outcome; \
             dependency-skipped, aborted, timed-out, and unreported work makes validation \
             incomplete and cannot report PASS."
        );
    }
    exit_code = exit_code_with_execution_completeness(exit_code, validation_complete);

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
    // Every receipt-producing profile plans `pre.reverie_pin` and every lane
    // node depends on it, so in principle a green receipt cannot happen without
    // it. This asserts that anyway: if a future fast path, cache branch, or early
    // return ever bypasses the pin gate, it must not emit PASS merely because the
    // tests it did select happened to pass. An off-the-record selected subgraph
    // is the explicit exception: it cannot write a ledger row or receipt, and
    // the external workflow already depends on the preflight result. The
    // archival pin is not a testing exemption, and "the DAG makes it impossible"
    // is a structural argument, not an observation of a receipt-producing run.
    let mut pin_gate_bypassed = false;
    if pin_gate_blocks_pass(
        exit_code,
        pin_gate_passed,
        args.allow_local_off_the_record_run,
    ) {
        eprintln!(
            "validate: ERROR: this path produced a PASS without a passing {PIN_GATE_TAG} gate; \
             refusing a passing receipt."
        );
        exit_code = 1;
        pin_gate_bypassed = true;
    }

    // A full top-level run must carry the exact per-cell population it just
    // judged. Older schema-5 rows could say only that buckets passed; they could
    // not open or satisfy a cell-specific failure obligation. Retain the typed
    // rows before appending the ledger entry so schema 7 is emitted only when
    // the artifact has actually been published and bound by checksum.
    let should_retain_cells = plan.suite_complete || plan.cell_evidence_expected.is_some();
    let retained_cell_results = if !nesting.nested
        && !args.allow_local_off_the_record_run
        && should_retain_cells
        && execution_complete
    {
        let expected = match &plan.cell_evidence_expected {
            Some(expected) => Ok(expected.clone()),
            None => validate_cell_results::expected_plan(&root),
        };
        let result = expected.and_then(|expected| {
            validate_cell_results::retain(
                parent.as_deref().unwrap_or(&root),
                &e2e_result_root,
                &commit,
                &expected,
            )
        });
        match result {
            Ok(results) => Some(results),
            Err(error) => {
                eprintln!(
                    "validate: ERROR: cannot retain complete per-cell evidence: {error}; \
                     refusing a schema-7 receipt"
                );
                exit_code = 1;
                None
            }
        }
    } else {
        None
    };
    let retained_coverage = if plan.suite_complete
        && !nesting.nested
        && !args.allow_local_off_the_record_run
    {
        let selected = retained_cell_results
            .as_ref()
            .and_then(|results| results.evidence.get("selected").and_then(serde_json::Value::as_array))
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| validate_cell_results::expected_plan(&root));
        let run_id = retained_cell_results
            .as_ref()
            .map(|results| results.run_id.clone())
            .or_else(|| {
                std::env::var("E2E_RUN_ID").ok().filter(|value| !value.trim().is_empty())
            })
            .ok_or("E2E_RUN_ID is missing after the validate run".to_string());
        match run_id.and_then(|run_id| {
            selected.and_then(|selected| {
                validate_cell_results::retain_coverage_evidence(
                    parent.as_deref().unwrap_or(&root),
                    &root,
                    &run_id,
                    &commit,
                    &plan.profile,
                    plan.selection_mode,
                    &planned_tags,
                    &plan.planned_test_nodes,
                    &coverage,
                    &selected,
                )
            })
        }) {
                Ok(scope) => {
                    let retained_plan = &scope.evidence["plan"];
                    let e2e = &scope.evidence["e2e"];
                    let binaries = &scope.evidence["integration_test_binaries"];
                    let plan_name = retained_plan["name"].as_str().unwrap_or("unknown");
                    let selected = e2e["selected_count"].as_u64().unwrap_or(0);
                    let enabled_not_selected =
                        e2e["enabled_not_selected_count"].as_u64().unwrap_or(0);
                    println!(
                        "Coverage: plan {} selected {} outer nodes; E2E selected {selected} of {} selected-or-enabled cells; \
                         {enabled_not_selected} enabled cells were not selected; integration \
                         binaries CI-registered {} of {} ({} reason-recorded, {} none-recorded).",
                        plan_name,
                        retained_plan["outer_node_count"],
                        selected + enabled_not_selected,
                        binaries["ci_registered_count"],
                        binaries["present_count"],
                        binaries["reason_recorded_count"],
                        binaries["none_recorded_count"],
                    );
                    println!("Coverage artifact: {}", scope.evidence["artifact"]["path"]);
                    Some(scope)
                }
                Err(error) => {
                    eprintln!(
                        "validate: ERROR: cannot retain complete coverage evidence: {error}; \
                         refusing a full receipt"
                    );
                    exit_code = 1;
                    None
                }
            }
    } else {
        None
    };
    let coverage = retained_coverage
        .as_ref()
        .map(|retained| retained.evidence.clone())
        .unwrap_or(coverage);
    // `--only` deliberately drops build dependencies, so a fast 127 there is
    // useful evidence of an absent prerequisite. It is still a red result and
    // only a possibility: exit 127 can also mean a missing host tool or typo.
    let missing_artifact = possible_missing_artifact_nodes(plan.selection_mode, &outcomes);
    if !missing_artifact.is_empty() {
        eprintln!(
            "validate: NOTE: {} --only node(s) exited 127 (command not found) in under 5s: {}. \
             Because --only drops outside dependencies, this MAY mean a required build artifact \
             is absent; it remains a RED test/configuration failure. Build the named dependencies \
             and re-run, or inspect the node command for a missing tool or typo.",
            missing_artifact.len(),
            missing_artifact.join(", ")
        );
    }

    // A NESTED payload writes nothing: the outer run owns the ledger and the
    // receipt, and a second row for one logical run is exactly the duplication
    // the re-entrancy guard exists to prevent.
    if !nesting.nested && !args.allow_local_off_the_record_run {
        write_ledger(
            &ledger,
            &ctx,
            &outcomes,
            &attempts,
            &skipped,
            &plan.host_inapplicable,
            &planned_tags,
            wall,
            exit_code,
            &log_path.to_string_lossy(),
            execution_complete,
            coverage,
            retained_cell_results.as_ref(),
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
        attribution_tree_dirty,
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

    // This must remain below the ledger append and receipt publication. Writing
    // the generated scorecard sooner changes the working tree while the run is
    // still establishing whether its receipt is commit-anchored. The checkout
    // lock remains held here, so another direct validate cannot start between
    // receipt finalization and this write-back. ci-hub additionally writes the
    // completed results back to the checkout that invoked the isolated run.
    let scorecard_writeback = local_scorecard_writeback(
        &root,
        &e2e_result_root,
        nesting.nested,
        args.allow_local_off_the_record_run,
    );

    // Read the individual results before removing the disposable build root: a
    // caller may deliberately place E2E_RESULT_ROOT there. The scheduler is
    // finished, and `read_log_since_settled` flushes the live tee before reading.
    let failed_finally = classification.product_failure_nodes.clone();
    let nextest_nodes = plan
        .cfg
        .steps
        .iter()
        .chain(plan.second.iter().flat_map(|config| config.steps.iter()))
        .filter(|step| step.cmd.contains("run-nextest-counted.sh"))
        .map(Step::tag)
        .collect::<BTreeSet<_>>();
    let (mut test_observations, mut test_summary_errors) =
        nextest_test_observations(&attempts, &nextest_nodes);
    match read_log_since_settled(&log_path, 0) {
        Some(log) => test_observations.extend(dbt_parity_test_observations(&log)),
        None => {
            test_summary_errors.push(
                "individual DBT test ids could not be read from the durable log; failed DAG nodes \
                 are listed separately below rather than mislabeled as test ids"
                    .to_string(),
            );
        }
    }
    match e2e_test_observations(&e2e_result_root) {
        Ok(mut observations) => test_observations.append(&mut observations),
        Err(error) => test_summary_errors.push(format!(
            "individual E2E test ids could not be read from the per-cell results: {error}; \
             failed DAG nodes are listed separately below rather than mislabeled as test ids"
        )),
    }
    let test_summary = test_id_summary(test_observations, &attempts, &failed_finally);

    drop(run_record);
    let _ = std::fs::remove_dir_all(&tmp);

    // The completed-run summary. Names the excused rows explicitly, so a green
    // verdict that ignored some failures can never read as "everything passed".
    let mut detail = vec![validation_completeness_detail(
        validation_complete, classification.product_result_nodes.len(), planned_tags.len(),
    )];
    let excused = failures.saturating_sub(effective_failures);
    if exit_code == 0 {
        detail.push(format!("every blocking gate passed ({} node(s) ran)", outcomes.len()));
    } else if exit_code == NO_RESULT_EXIT_CODE as u8 {
        detail.push(format!(
            "{} gate(s) could not determine their condition: {}",
            no_results,
            no_result_nodes.join(", ")
        ));
    } else {
        let (_named, listing) = classified_blocking_listing(
            &classification, &summary_nonblocking, effective_failures,
        );
        detail.push(format!("{effective_failures} blocking failure(s){listing}"));
    }
    if exit_code != NO_RESULT_EXIT_CODE as u8 && no_results > 0 {
        detail.push(format!(
            "{} gate(s) reported NO_RESULT but did not hide the genuine failure(s): {}",
            no_results,
            no_result_nodes.join(", ")
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
    if let Some(error) = &compatibility_count_error {
        detail.push(format!(
            "direct compatibility rows could not produce an exact test count: {error}"
        ));
    }
    if !timed_out_nodes(&outcomes).is_empty() {
        detail.push(format!(
            "{} node(s) hit a wall or CPU budget; a timeout IS a recorded result: {}",
            timed_out_nodes(&outcomes).len(),
            timed_out_nodes(&outcomes).join(", ")
        ));
    }
    detail.extend(execution_completeness_details(&skipped, execution_complete));
    // Named in the verdict itself, not only in the plan header. A green summary
    // that omitted this would let a reader take the run for full coverage.
    if !plan.host_inapplicable.is_empty() {
        detail.push(format!(
            "{} planned node(s) were NOT RUN because this machine provably cannot run them, and \
             are recorded as '{}': {}. This is NOT a pass and NOT coverage — whatever those nodes \
             verify is UNVERIFIED by this run, and the ledger row carries the omission so the \
             parent's receipt gate can refuse it.",
            plan.host_inapplicable.len(),
            validate_plan::HOST_INAPPLICABLE_REASON,
            plan.host_inapplicable
                .iter()
                .map(|n| format!("{} (needs {})", n.tag, n.capability.value()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
    match executed_tests {
        Some(n) => detail.push(format!(
            "{n} test(s) executed, {} passed, {} filtered (aggregated from typed step outcomes)",
            passed_tests.map(|p| p.to_string()).unwrap_or_else(|| "unknown".into()),
            filtered_tests.map(|f| f.to_string()).unwrap_or_else(|| "unknown".into())
        )),
        None => detail.push(
            "executed_tests is UNKNOWN — this row is a NON-VERDICT and cannot qualify a receipt, \
             whatever the exit code says"
                .into(),
        ),
    }
    let individual_test_results_complete = test_summary_errors.is_empty();
    detail.extend(test_summary_errors);
    if args.allow_local_off_the_record_run {
        detail.push(
            "this was local iterative testing off the record: no ledger row or receipt was \
             published, and the result cannot be cited as validation evidence"
                .into(),
        );
    }

    let mut s = RunSummary::new(
        match exit_code {
            0 => Verdict::Pass,
            code if code == NO_RESULT_EXIT_CODE as u8 => Verdict::NoResult,
            _ => Verdict::Fail,
        },
        exit_code,
        &plan.profile,
        detail,
    );
    s.nodes_executed = completed_node_count(&outcomes, &attempts);
    s.nodes_failed = failures;
    s.flaky = test_summary.recovered;
    s.failed_ids = test_summary.failed;
    s.failed_nodes_without_test_ids = test_summary.failed_nodes_without_test_ids;
    s.retry_occurrences = test_summary.retry_occurrences;
    s.individual_test_results_complete = individual_test_results_complete;
    s.nodes_skipped = skipped.len();
    s.nodes_host_inapplicable = plan.host_inapplicable.len();
    s.executed_tests = executed_tests;
    s.passed_tests = passed_tests;
    s.selection_mode = Some(plan.selection_mode.into());
    s.wall_s = Some(wall);
    s.jobs = Some(jobs);
    s.log = Some(log_path);
    s.cpu_wall = Some((wall, cpu_user, cpu_sys));
    if !nesting.nested && !args.allow_local_off_the_record_run {
        s.ledger = Some(ledger);
    }
    record_scorecard_writeback(&mut s, scorecard_writeback);
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
fn stop_test_seam(
    root: &Path,
    profile: &str,
    parent: Option<&Path>,
    tool_root: Option<&Path>,
    off_the_record: bool,
) -> RunSummary {
    let started_at = utc_now();
    let started = std::time::Instant::now();
    let prior_failure = env_flag("VALIDATE_STOP_TEST_PRIOR_FAILURE", "1");
    let synth = |name: &str, ok: bool| StepOutcome {
        tag: name.to_string(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        test_results: None,
        returncode: Some(if ok { 0 } else { 1 }),
        oomed: false,
        oom_kills: 0,
        timed_out: false,
        cpu_timed_out: false,
        reason: if ok { String::new() } else { "stop-test synthetic failure".into() },
        aborted: false,
    };
    let outcomes =
        vec![synth("stop-test completed gate 1", !prior_failure), synth("stop-test completed gate 2", true)];

    let commit = git_sha();
    let git_depth = match measure_git_depth(&commit) {
        Ok(depth) => depth,
        Err(error) => {
            return RunSummary::refused(
                2,
                profile,
                "git depth measurement",
                vec![
                    error,
                    "the schema requires a real git_depth; refusing instead of omitting it or inventing a value"
                        .into(),
                ],
            )
        }
    };

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
    let lock_admitted = validate_lock_admission(tool_root, &commit, &host).is_ok();
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
        git_depth,
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
        retry_rounds: 0,
        executed_tests: None,
        passed_tests: None,
        filtered_tests: None,
    };
    // `execution_complete: false` keeps this synthetic fixture incomplete. Its
    // selected denominator is retained without claiming a completed full profile.
    // The fixture plans exactly the synthetic gates it ran, withholds nothing,
    // and leaves nothing unaccounted.
    let planned_tags: BTreeSet<String> = outcomes.iter().map(|o| o.tag.clone()).collect();
    if !off_the_record {
        write_ledger(
            &ledger,
            &ctx,
            &outcomes,
            &[],
            &[],
            &[],
            &planned_tags,
            wall,
            exit_code,
            "",
            false,
            serde_json::json!({}),
            None,
        );
    }

    let mut detail = match exit {
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
    if off_the_record {
        detail.push(
            "stop-path fixture ran OFF THE RECORD: no ledger row, receipt, scorecard, or label was published"
                .into(),
        );
    }
    let mut s = RunSummary::new(
        if interruption.is_some() { Verdict::Interrupted } else { Verdict::Fail },
        exit_code,
        profile,
        detail,
    );
    s.nodes_executed = completed_node_count(&outcomes, &[]);
    s.nodes_failed = outcomes.iter().filter(|o| !o.ok).count();
    s.wall_s = Some(wall);
    s.cpu_wall = Some((wall, cpu_user, cpu_sys));
    if !off_the_record {
        s.ledger = Some(ledger);
    }
    s
}

#[cfg(test)]
mod committed_selection_preservation_tests {
    use super::*;

    fn inert_step(source: &Step, command: String) -> Step {
        let mut step = step_with_caps(
            &source.group, &source.job, "committed dependency/resource fixture",
            command, source.deps.clone(), 10, 10, 64 * 1024 * 1024,
        );
        step.hint.resources = source.hint.resources.clone();
        step.fail_fast_family = source.fail_fast_family.clone();
        step
    }

    #[test]
    fn focused_selection_cannot_execute_after_failed_preflight() {
        let root = Path::new(file!()).parent().and_then(Path::parent).expect("validate.rs has a repository parent");
        for (lane, target, pin, gate) in [
            ("portable", "test.detcore_unit", PIN_GATE_TAG, "gate.manifest"),
            ("hosted-portable", "test.cli_on_host", PIN_GATE_TAG, "gate.manifest"),
            ("hosted-privileged", "privileged-only-test.cli_kvm_on_host", "pre.reverie_pin_on_host", "gate.manifest_on_host"),
        ] {
        for off_record in [false, true] {
            for failed_gate in [pin, gate] {
                if off_record && failed_gate == gate { continue; }
                let temp = tempfile::tempdir().unwrap();
                let sentinel = temp.path().join("target-ran");
                let mut argv = vec!["--only".into(), lane.into(), target.into()];
                if off_record { argv.push(ALLOW_LOCAL_OFF_THE_RECORD_RUN_OPTION.into()); }
                let args = parse_argv(&argv).unwrap();
                let plan = build_plan(root, &args, temp.path()).unwrap();
                assert!(plan.cfg.steps.iter().any(|step| step.tag() == failed_gate));
                let fixture = plan.cfg.with_steps(plan.cfg.steps.iter().map(|source| {
                    let cmd = match source.tag().as_str() {
                        tag if tag == failed_gate => "exit 23".into(),
                        tag if tag == target => format!(": > {}", validate_plan::shell_quote(&sentinel.to_string_lossy())),
                        _ => "true".into(),
                    };
                    inert_step(source, cmd)
                }).collect());
                let result = run_lane_once(&fixture, 4, true, 0, None, &temp.path().join("failed.log"), None, false);
                assert!(!result.ok);
                assert!(result.outcomes.iter().any(|outcome| outcome.tag == failed_gate && outcome.returncode == Some(23)));
                assert!(result.skipped.contains(&target.to_string()), "{:?}", result.skipped);
                assert!(!sentinel.exists(), "{failed_gate} failure did not block target (off_record={off_record})");

                // Removing both gate edges recreates the bug and must let the
                // sentinel run despite the same failing preflight.
                let mut missing_gate = fixture.clone();
                missing_gate.steps.iter_mut().find(|step| step.tag() == target).unwrap().deps.clear();
                let result = run_lane_once(&missing_gate, 4, true, 0, None, &temp.path().join("missing-edge.log"), None, false);
                assert!(!result.ok);
                assert!(sentinel.exists(), "negative control failed to expose lost ordering");
            }
        }
    }
    }

    #[test]
    fn portable_failure_preserves_independent_privileged_work_and_shared_exclusion() {
        let root = Path::new(file!()).parent().and_then(Path::parent).expect("validate.rs has a repository parent");
        let committed = validate_plan::validation_config(root).unwrap();
        let tags = ["test.cli", "privileged-build.privileged_tests", "privileged-test.cli_kvm"];
        let selected = dagrun::select_steps_by_tags(&committed, &tags.iter().map(|tag| tag.to_string()).collect::<Vec<_>>(), true).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let active = validate_plan::shell_quote(&temp.path().join("active").to_string_lossy());
        let artifact = validate_plan::shell_quote(&temp.path().join("artifact").to_string_lossy());
        let passed = temp.path().join("privileged-passed");
        let fixture = selected.with_steps(selected.steps.iter().map(|source| {
            let command = match source.tag().as_str() {
                "test.cli" => format!("set -eu; mkdir {active}; sleep 0.1; rmdir {active}; exit 17"),
                "privileged-build.privileged_tests" => format!("set -eu; mkdir {active}; sleep 0.1; : > {artifact}; rmdir {active}"),
                "privileged-test.cli_kvm" => format!("set -eu; mkdir {active}; test -f {artifact}; : > {}; rmdir {active}", validate_plan::shell_quote(&passed.to_string_lossy())),
                _ => unreachable!(),
            };
            inert_step(source, command)
        }).collect());
        let result = run_lane_once(&fixture, 3, true, 0, None, &temp.path().join("keep-going.log"), None, false);
        assert!(!result.ok);
        assert!(result.skipped.is_empty(), "{:?}", result.skipped);
        assert_eq!(result.outcomes.len(), 3);
        assert!(result.outcomes.iter().any(|outcome| outcome.tag == "test.cli" && outcome.returncode == Some(17)));
        for tag in &tags[1..] {
            assert!(result.outcomes.iter().any(|outcome| outcome.tag == *tag && outcome.ok), "{tag} did not finish independently");
        }
        assert!(passed.is_file());

        // Reintroducing the rejected success dependency must skip both
        // privileged nodes while leaving the portable failure unchanged.
        std::fs::remove_file(&passed).unwrap();
        let mut success_dependency = fixture;
        success_dependency.steps.iter_mut().find(|step| step.tag() == "privileged-build.privileged_tests").unwrap().deps.push("test.cli".into());
        let result = run_lane_once(&success_dependency, 3, true, 0, None, &temp.path().join("success-edge.log"), None, false);
        assert!(!result.ok);
        assert!(!passed.exists());
        for tag in &tags[1..] { assert!(result.skipped.contains(&tag.to_string())); }
    }
}

#[cfg(test)]
mod fused_privileged_build_tests {
    use super::*;
    include!("../ci/cargo-guest-binaries.rs");

    fn write_executable(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn cold_fixture(repository: &Path, helper: &Path) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let cargo_log = root.path().join("cargo-calls");
        write_executable(&root.path().join("ci/verify-hermit-e2e-artifact.sh"), "#!/bin/sh\nexit 0\n");
        write_executable(&root.path().join("ci/run-with-reverie-dbt-budget.sh"), "#!/bin/sh\nexec \"$@\"\n");
        write_executable(&root.path().join("bin/cargo"), include_str!("../ci/tests/nextest-preparation-cargo-fixture.py"));
        std::os::unix::fs::symlink(helper, root.path().join("ci/nextest-binaries.rs")).unwrap();
        std::fs::create_dir_all(root.path().join("ci/dag")).unwrap();
        std::fs::copy(repository.join("ci/dag/validate.json"), root.path().join("ci/dag/validate.json")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(root.path().join("guest-names.json"), serde_json::to_vec(&CARGO_GUEST_BINARIES).unwrap()).unwrap();
        std::fs::write(root.path().join(".gitignore"), "/target/\n/custom-cargo-target/\n/cargo-calls\n").unwrap();
        for args in [vec!["init", "-q"], vec!["add", "."], vec!["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]] {
            let output = Command::new("git").args(args).current_dir(root.path()).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        let bin = root.path().join("bin");
        (root, bin, cargo_log)
    }

    fn run_build(command: &str, root: &Path, bin: &Path, cargo_log: &Path, artifact_mode: &str) -> std::process::Output {
        let mut path = vec![bin.to_path_buf()];
        if let Some(existing) = std::env::var_os("PATH") { path.extend(std::env::split_paths(&existing)); }
        Command::new("bash").arg("-c").arg(command).current_dir(root)
            .env("PATH", std::env::join_paths(path).unwrap())
            .env("CARGO_CALL_LOG", cargo_log).env("CARGO_ARTIFACT_MODE", artifact_mode)
            .output().unwrap()
    }

    #[test]
    fn fused_privileged_build_creates_tests_misc_in_a_cold_target_before_consumers() {
        let repository = Path::new(file!()).parent().and_then(Path::parent).unwrap();
        let committed = validate_plan::validation_config(repository).unwrap();
        let workspace = committed.steps.iter().find(|step| step.tag() == "build.workspace").unwrap();
        assert!(workspace.cmd.ends_with(NEXTEST_PORTABLE_PREPARE_COMMAND));
        let preparation = &workspace.cmd[workspace.cmd.len() - NEXTEST_PORTABLE_PREPARE_COMMAND.len()..];
        let consumer = committed.steps.iter().find(|step| step.tag() == "privileged-build.privileged_tests").unwrap();
        assert!(!consumer.cmd.contains("cargo "), "the barrier must not rebuild shared test executables");
        let query = Command::new(repository.join("ci/nextest-binaries.rs")).arg("--print-executable").output().unwrap();
        assert!(query.status.success(), "{}", String::from_utf8_lossy(&query.stderr));
        let helper = PathBuf::from(String::from_utf8(query.stdout).unwrap().trim());
        assert!(helper.is_absolute() && helper.is_file());

        let (root, bin, log) = cold_fixture(repository, &helper);
        let unprepared = run_build(&consumer.cmd, root.path(), &bin, &log, "current");
        assert!(!unprepared.status.success(), "a cold consumer cannot invent the missing preparation");
        assert!(!log.exists(), "the consumer must not fall back to Cargo");
        let prepared = run_build(preparation, root.path(), &bin, &log, "current");
        assert!(prepared.status.success(), "{}", String::from_utf8_lossy(&prepared.stderr));
        let before = std::fs::read_to_string(&log).unwrap();
        let expected = hermit_manifest_plan::nextest_binaries::profile_selections(repository, "portable").unwrap();
        let calls = before.lines().map(|line| serde_json::from_str::<Vec<String>>(line).unwrap()).collect::<Vec<_>>();
        let builds = calls.iter().filter(|args| args.first().map(String::as_str) == Some("nextest") && !args.iter().any(|arg| arg == "--binaries-metadata")).collect::<Vec<_>>();
        assert_eq!(builds.len(), expected.len(), "each distinct selection is prepared once");
        for selection in expected.values() {
            assert_eq!(builds.iter().filter(|args| args.ends_with(selection)).count(), 1, "missing or duplicated selection {selection:?}");
        }
        assert_eq!(calls.iter().filter(|args| args.first().map(String::as_str) == Some("build") && args.iter().any(|a| a == "hermetic_infra_hermit_tests")).count(), 1, "the 21 Cargo guests are built together once");
        assert_eq!(calls.iter().filter(|args| args.first().map(String::as_str) == Some("build") && args.iter().any(|a| a == "nextest-cpu-wrapper")).count(), 1, "the normal measurement wrapper is built once, outside every test consumer");
        assert_eq!(calls.iter().filter(|args| args.first().map(String::as_str) == Some("build")).count(), 2, "no other builds were introduced");
        let read_only = run_build(&consumer.cmd, root.path(), &bin, &log, "current");
        assert!(read_only.status.success(), "{}", String::from_utf8_lossy(&read_only.stderr));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "the privileged consumer must not invoke Cargo after preparation");
        let record_path = root.path().join("target/ci/nextest-binaries/current.json");
        let published_record = std::fs::read(&record_path).unwrap();
        let wrapper_query = run_build("./ci/nextest-binaries.rs cpu-wrapper", root.path(), &bin, &log, "current");
        assert!(wrapper_query.status.success(), "{}", String::from_utf8_lossy(&wrapper_query.stderr));
        let wrapper = PathBuf::from(String::from_utf8(wrapper_query.stdout).unwrap().trim());
        assert_eq!(wrapper, root.path().join("custom-cargo-target/debug/nextest-cpu-wrapper"));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "a wrapper lookup must not build");
        let wrapper_bytes = std::fs::read(&wrapper).unwrap();
        for mode in ["missing", "stale"] {
            if mode == "missing" { std::fs::remove_file(&wrapper).unwrap(); }
            else { std::fs::write(&wrapper, "#!/bin/sh\nexit 23\n").unwrap(); }
            assert!(!run_build("./ci/nextest-binaries.rs cpu-wrapper", root.path(), &bin, &log, "current").status.success(), "accepted {mode} wrapper");
            assert!(!run_build(&consumer.cmd, root.path(), &bin, &log, "current").status.success(), "accepted {mode} wrapper through the barrier");
            assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "a {mode} wrapper cannot cause a fallback build");
            write_executable(&wrapper, std::str::from_utf8(&wrapper_bytes).unwrap());
            // write_executable uses 0700; preserve the producer's original mode.
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let no_build = run_build("HERMIT_PREPARED_NEXTEST_REQUIRED=1 ./ci/nextest-binaries.rs build-cpu-wrapper", root.path(), &bin, &log, "current");
        assert!(!no_build.status.success(), "an official consumer cannot invoke standalone preparation");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before);
        let selection = expected.values().next().unwrap();
        let declaration = validate_plan::shell_quote(&serde_json::to_string(selection).unwrap());
        let selectors = selection.iter().map(|arg| validate_plan::shell_quote(arg)).collect::<Vec<_>>().join(" ");
        for operation in ["list", "run"] {
            let cmd = format!("HERMIT_PREPARED_NEXTEST_REQUIRED=1 NEXTEST_PREPARED_BUILD_SELECTION={declaration} HERMIT_NEXTEST_CPU_WRAPPER_BIN={} ./ci/nextest-binaries.rs {operation} {selectors}", validate_plan::shell_quote(&wrapper.to_string_lossy()));
            let result = run_build(&cmd, root.path(), &bin, &log, "current");
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        }
        let after_readers = std::fs::read_to_string(&log).unwrap();
        let added = after_readers.strip_prefix(&before).unwrap().lines().map(|line| serde_json::from_str::<Vec<String>>(line).unwrap()).collect::<Vec<_>>();
        assert_eq!(added.len(), 2);
        for (args, operation) in added.iter().zip(["list", "run"]) {
            assert_eq!(&args[..2], ["nextest", operation]);
            assert!(args.iter().any(|a| a == "--cargo-metadata") && args.iter().any(|a| a == "--binaries-metadata"));
        }
        let declined = run_build(preparation, root.path(), &bin, &log, "declined");
        assert_eq!(declined.status.code(), Some(75), "a declined Cargo preparation must remain no-result");
        assert_eq!(std::fs::read(&record_path).unwrap(), published_record, "a declined replacement must preserve the prior complete record");
        let before = std::fs::read_to_string(&log).unwrap();
        assert!(run_build(&consumer.cmd, root.path(), &bin, &log, "current").status.success(), "unchanged prior preparation must remain usable after a declined replacement");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "using the prior preparation must not invoke Cargo");
        let executable = run_build("./ci/nextest-binaries.rs executable hermit-detcore tests_misc", root.path(), &bin, &log, "current");
        assert!(executable.status.success());
        let executable = PathBuf::from(String::from_utf8(executable.stdout).unwrap().trim());
        assert_eq!(executable, root.path().join("custom-cargo-target/debug/build/hermit-detcore/out/tests_misc"));
        assert!(executable.is_file(), "the exact Cargo target must exist before the consumer");
        assert!(!root.path().join("target/debug").exists(), "the producer must not silently remap to the default target");
        let changed_build = run_build(&format!("export SOURCE_DATE_EPOCH=946684800; {}", consumer.cmd), root.path(), &bin, &log, "current");
        assert!(!changed_build.status.success(), "a changed build timestamp must refuse prepared metadata");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "a stale build setting must not compile a replacement");
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n# changed source\n").unwrap();
        assert!(!run_build(&consumer.cmd, root.path(), &bin, &log, "current").status.success(), "changed source must refuse old metadata");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "a stale source must not compile a replacement");
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        assert!(run_build(&consumer.cmd, root.path(), &bin, &log, "current").status.success(), "restoring exact inputs must restore acceptance");
        std::fs::write(&executable, "#!/bin/sh\nexit 17\n").unwrap();
        assert!(!run_build(&consumer.cmd, root.path(), &bin, &log, "current").status.success(), "changed bytes at the same path must be stale");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "stale refusal must not compile a replacement");

        // The standalone privileged lane must prepare tests_misc even though
        // CPUID executes its harness directly rather than through Nextest.
        let (privileged_root, privileged_bin, privileged_log) = cold_fixture(repository, &helper);
        let privileged = run_build("./ci/nextest-binaries.rs prepare privileged", privileged_root.path(), &privileged_bin, &privileged_log, "current");
        assert!(privileged.status.success(), "{}", String::from_utf8_lossy(&privileged.stderr));
        let privileged_selections = hermit_manifest_plan::nextest_binaries::profile_selections(repository, "privileged").unwrap();
        assert_eq!(privileged_selections.len(), 3);
        assert!(privileged_selections.values().any(|args| args == &["-p", "hermit-detcore", "--test", "tests_misc"]));
        let direct = committed.steps.iter().find(|step| step.tag() == "privileged-only-cpuid.faulting").unwrap();
        let declaration = direct.env.get(hermit_manifest_plan::nextest_binaries::SELECTION_ENV).unwrap();
        let prefix = format!("export HERMIT_PREPARED_NEXTEST_REQUIRED=1; export NEXTEST_PREPARED_BUILD_SELECTION={}; ", validate_plan::shell_quote(declaration));
        let before = std::fs::read_to_string(&privileged_log).unwrap();
        let direct_result = run_build(&format!("{prefix}{}", direct.cmd), privileged_root.path(), &privileged_bin, &privileged_log, "current");
        assert!(direct_result.status.success(), "{}", String::from_utf8_lossy(&direct_result.stderr));
        let missing_declaration = run_build("export HERMIT_PREPARED_NEXTEST_REQUIRED=1; unset NEXTEST_PREPARED_BUILD_SELECTION; ./ci/nextest-binaries.rs executable hermit-detcore tests_misc", privileged_root.path(), &privileged_bin, &privileged_log, "current");
        assert!(!missing_declaration.status.success(), "the required direct selection cannot be omitted");
        let wrong_declaration = run_build("export HERMIT_PREPARED_NEXTEST_REQUIRED=1; export NEXTEST_PREPARED_BUILD_SELECTION='[\"-p\",\"hermit-detcore\",\"--lib\"]'; ./ci/nextest-binaries.rs executable hermit-detcore tests_misc", privileged_root.path(), &privileged_bin, &privileged_log, "current");
        assert!(!wrong_declaration.status.success(), "a different build selection cannot satisfy the direct target");
        assert_eq!(std::fs::read_to_string(&privileged_log).unwrap(), before, "direct consumers and refusals must not invoke Cargo");

        for mode in ["missing", "wrong", "ambiguous", "wrapper-missing", "wrapper-wrong", "wrapper-ambiguous"] {
            let (root, bin, log) = cold_fixture(repository, &helper);
            let result = run_build(preparation, root.path(), &bin, &log, mode);
            assert!(!result.status.success(), "the actual producer accepted Cargo artifact mode {mode}");
            assert!(!root.path().join("target/ci/nextest-binaries/current.json").exists(), "a failed producer must not publish partial selections");
            let before = std::fs::read_to_string(&log).unwrap();
            assert!(!run_build(&consumer.cmd, root.path(), &bin, &log, mode).status.success());
            assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "failed preparation must not enable consumer compilation");
        }
    }
}


#[cfg(test)]
mod e2e_attempt_tests {
    use super::*;

    fn manifest_step(job: &str) -> Step {
        let mut step = step_with_caps(
            "e2e",
            job,
            "fixture",
            "target/debug/test-harness run --lane portable --ci-only".into(),
            Vec::new(),
            30,
            30,
            64 * 1024 * 1024,
        );
        step.manifest = Some(DagManifest {
            lane: "portable".into(),
            category: "applications".into(),
            test: None,
            mode: None,
            backend: None,
        });
        step
    }

    #[test]
    fn no_lane_step_receives_an_outer_attempt_environment_variable() {
        let manifest = manifest_step("manifest_applications");
        assert_eq!(validation_step_identity(&manifest), ValidationStepIdentity::ManifestRun);
        assert!(!manifest.env.contains_key("E2E_ATTEMPT"));

        let ordinary = step_with_caps(
            "test",
            "unit",
            "fixture",
            "cargo test".into(),
            Vec::new(),
            30,
            30,
            64 * 1024 * 1024,
        );
        assert_eq!(validation_step_identity(&ordinary), ValidationStepIdentity::Other);
        assert!(!ordinary.env.contains_key("E2E_ATTEMPT"));
    }

}

#[cfg(test)]
mod typed_termination_tests {
    use super::*;

    #[test]
    fn typed_budget_and_oom_facts_survive_presentation_precedence() {
        budget_reason_bracket().unwrap();
    }

    #[test]
    fn gate_and_attempt_termination_fields_preserve_latest_unknown() {
        ledger_gate_origin_bracket().unwrap();
    }

    #[test]
    fn emitted_gate_rows_round_trip_through_the_shared_parent_reader() {
        for row in typed_gate_round_trip_bracket().unwrap() {
            println!(
                "TYPED_GATE_FIXTURE {}",
                serde_json::to_string(&row).unwrap()
            );
        }
    }
}
