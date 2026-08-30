#!/usr/bin/env -S rust-script --force
//! Safely retry red compatibility cells and repeat one committed green cell.
//!
//! ```cargo
//! [dependencies]
//! chrono = "0.4"
//! csv = "1"
//! dagrun = { path = "../../agent-utils/rs/dagrun" }
//! hermit-manifest-plan = { path = "../manifest-plan" }
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! sha2 = "0.10"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "../../scripts/lib/safe_ci_scope.rs"]
mod safe_ci_scope;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::io::Write as _;
use std::process::Command;
use std::process::Stdio;
use std::process::ExitCode;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::LOG_DIR_ENV;
use dagrun::NO_LOGS_ENV;
use dagrun::attribution::sanitize as sanitize_step_tag;
use dagrun::io::dag_from_json;
use dagrun::io::dag_to_json;
use dagrun::model::CmdType;
use dagrun::model::DEFAULT_CPU_TIMEOUT_MULTIPLIER;
use dagrun::model::DagConfig;
use dagrun::model::ResourceHint;
use dagrun::model::ResultManifest;
use dagrun::model::RunResult;
use dagrun::model::Step;
use dagrun::model::StepClass;
use dagrun::model::StepOutcome;
use dagrun::model::StructuredTestResultsManifest;
use dagrun::model::effective_cpu_count;
use dagrun::model::effective_cpu_timeout;
use dagrun::cgroup::aggregate_slice_max_cpus;
use dagrun::box_mem_budget_bytes;
use dagrun::container_core_budget;
use dagrun::LOG_DIR_ENV as RUNNER_LOG_DIR_ENV;
use dagrun::NO_LOGS_ENV as RUNNER_NO_LOGS_ENV;
use dagrun::scheduler::BoxedCgroups;
use dagrun::scheduler::run_dag_boxed_deadline;
use hermit_manifest_plan::canonical_verdict::NoResultReason;
use hermit_manifest_plan::canonical_verdict::RuntimeStats;
use hermit_manifest_plan::canonical_verdict::VerificationReport;
use hermit_manifest_plan::canonical_verdict::VerificationRuntime;
use hermit_manifest_plan::canonical_verdict::Verdict;
use hermit_manifest_plan::environmental_block::EnvBlockClass;
use hermit_manifest_plan::environmental_block::EnvBlockObservation;
use hermit_manifest_plan::environmental_block::environmental_block_observation;
use hermit_manifest_plan::host_capability::CapabilityVerdict;
use hermit_manifest_plan::host_capability::HostCapability;
use hermit_manifest_plan::runner::AttemptResult;
use hermit_manifest_plan::runner::CELL_RESULT_SCHEMA;
use hermit_manifest_plan::runner::CellResult;
use hermit_manifest_plan::runner::cell_result_and_attempts_after_retries;
use hermit_manifest_plan::runner::cell_result_after_retries;
use hermit_manifest_plan::runner::E2E_RUN_INDEX_ENV;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::ObservedResult;
use hermit_manifest_plan::runner::MAX_ATTEMPTS_PER_CELL;
use hermit_manifest_plan::stress_series::SeriesNoVerdictKind;
use hermit_manifest_plan::stress_series::SeriesPressureAttempt;
use hermit_manifest_plan::stress_series::SeriesPressureComparison;
use hermit_manifest_plan::timeouts::TimeoutMultipliers;
use hermit_manifest_plan::timeouts::resolve_test_timeouts;
use hermit_manifest_plan::timeouts::timeout_multipliers_from_env;
use hermit_manifest_plan::timeouts::validate_timeout_multiplier;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

const TRACKED_CELLS: &str = "ci/compat-envelope/cells.json";
const PORTABLE_DAG: &str = "ci/dag/validate.json";
/// ⚠️ COUPLED TO `SCHEMA` IN ci/compat-envelope/scorecard.rs. Both tools read
/// cells.json and both pin its version, so a bump in one WITHOUT the other
/// leaves this tool refusing every tracked file with "unsupported tracked cell
/// schema N". That is a fail-closed refusal rather than silent misreading, but
/// it takes the pressure test offline entirely, and nothing in either file
/// points at the other -- which is how it was missed when 5 became 6.
const TRACKED_CELLS_SCHEMA: u64 = 7;
const RUN_SCHEMA: u64 = 3;
const SUMMARY_SCHEMA: u64 = 5;
const RUNNER_STEP_OUTPUT_DIR: &str = "runner-profile";
const PROMOTION_REPETITIONS: usize = 10;
const REQUIRED_BUILD_TAGS: [&str; 10] = [
    "pre.submodules",
    "pre.reverie_pin",
    "build.rust_scripts",
    "setup.manifest_plan",
    "setup.nextest",
    "gate.manifest",
    "build.workspace",
    "build.runtime_release",
    "build.e2e_artifact",
    "build.liteinst_runtime_release",
];
/// Written before a cell starts. If the cell's cgroup is killed before the
/// harness can report, this remains a conservative non-pass attempt marker.
const INCOMPLETE_ATTEMPT_STATUS: i32 = 125;
/// Written when a cell is not invoked because its serialized fixture
/// preparation did not complete successfully.
const PREPARATION_FAILED_STATUS: i32 = 126;
/// Historical pressure plans truncated their enclosing cell allowance at 600s.
/// Retained plans without a timeout-policy record still use that exact rule.
const LEGACY_PRESSURE_CELL_TIMEOUT_SECONDS: i64 = 600;
const MAX_PRESSURE_GENERATED_NODES: usize = 100_000;
/// The prior 432-cell measurement completed in nine minutes on this host. Two
/// hours is an operational stop for the periodic experiment, not a pass
/// threshold: breach makes the run incomplete and publishes no promotion.
const PRESSURE_RUN_TIMEOUT_SECONDS: i64 = 2 * 60 * 60;
const PRESSURE_SCOPE_TIMEOUT_ENV: &str = "HERMIT_PRESSURE_SCOPE_TIMEOUT_SECONDS";
const HERMETIC_TEST_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
const DEFAULT_MANIFEST_GUEST_CAP: i64 = 4;
const DEFAULT_KVM_GUEST_CAP: i64 = 4;
const PORTABLE_CELL_MEMORY_BYTES: i64 = 3 * 1024 * 1024 * 1024;
const PRIVILEGED_CELL_MEMORY_BYTES: i64 = 16 * 1024 * 1024 * 1024;
const PREPARATION_MEMORY_BYTES: i64 = 3 * 1024 * 1024 * 1024;
const CONTROL_PLANE_HEADROOM_BYTES: i64 = 1024 * 1024 * 1024;

/// Match validate's measured host-adaptive outer scheduling policy.
///
/// Pressure previously used a literal width of four with no host or workload
/// evidence. That made the same cell population run under an arbitrary outer
/// contention policy merely because it was selected as red rather than green.
/// Population selection may differ; execution scheduling should not.
fn default_jobs() -> i64 {
    if let Ok(value) = env::var("CI_DAG_JOBS") {
        if let Ok(jobs) = value.parse::<i64>() {
            if jobs > 0 {
                return jobs;
            }
        }
        eprintln!(
            "pressure-test: CI_DAG_JOBS={value:?} is not a positive integer; using the host-adaptive default"
        );
    }
    let host = std::thread::available_parallelism()
        .map(|count| count.get() as i64)
        .unwrap_or(1);
    (host / 8).clamp(2, 16)
}

fn pressure_scope_grace_s(run_timeout_s: i64) -> i64 {
    60.max(run_timeout_s / 10)
}

fn inherited_pressure_scope_timeout(run_timeout_s: i64) -> Result<Option<i64>, String> {
    parse_pressure_scope_timeout(run_timeout_s, env::var(PRESSURE_SCOPE_TIMEOUT_ENV))
}

fn parse_pressure_scope_timeout(
    run_timeout_s: i64, raw: Result<String, env::VarError>,
) -> Result<Option<i64>, String> {
    let inherited = match raw {
        Ok(raw) => Some(raw.parse::<i64>().map_err(|_| {
            format!("{PRESSURE_SCOPE_TIMEOUT_ENV}={raw:?} is not a valid positive timeout")
        })?),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(format!("{PRESSURE_SCOPE_TIMEOUT_ENV} is not valid UTF-8"));
        }
    };
    if let Some(inherited) = inherited {
        if inherited <= 0 || inherited != run_timeout_s {
            return Err(format!(
                "{PRESSURE_SCOPE_TIMEOUT_ENV}={inherited} does not match the requested {run_timeout_s}s whole-run bound"
            ));
        }
    }
    Ok(inherited)
}

fn establish_pressure_cgroups(run_timeout_s: i64) -> Result<BoxedCgroups, String> {
    let already_in_scope = dagrun::cgroup::is_in_scope();
    let inherited_marker = inherited_pressure_scope_timeout(run_timeout_s)?;
    if !already_in_scope {
        env::set_var(PRESSURE_SCOPE_TIMEOUT_ENV, run_timeout_s.to_string());
    }
    let owns_runtime = inherited_marker == Some(run_timeout_s) || !already_in_scope;
    let scope_runtime_s = Some(
        run_timeout_s
            .checked_add(pressure_scope_grace_s(run_timeout_s))
            .ok_or("--run-timeout is too large to establish a scope backstop")?,
    );
    safe_ci_scope::propagate_result(safe_ci_scope::resolve_cgroups(
        "compatibility pressure test",
        false,
        scope_runtime_s,
        owns_runtime,
    ))
    .map_err(|code| format!("cgroup setup refused with exit {code}"))
}

const USAGE: &str = r#"Hermit compatibility pressure test

Ordinary `validate` reruns the committed green compatibility cells and fails on
regressions. This tool probes currently red cells by default and can repeat
either red or explicitly selected green cells. Every check runs under safe-ci
resource and time limits and retains its raw evidence under ignored/. Red cells
remain red unless a later reviewed scorecard change deliberately promotes them;
repeated results never edit the scorecard.

Usage: ci/compat-envelope/pressure-test.rs COMMAND [OPTIONS]

Commands:
  run [--results DIR] [--mode MODE] [--sample COUNT] [--seed SEED]
      [--cells-file PATH]
      [--green --backend BACKEND --repetitions COUNT] [--jobs COUNT]
      [--probe-disabled --backend BACKEND]
      Run bounded probes for the selected red cells. An exact-cell run uses the
      current working tree for fast fix/test iteration; a dirty result is
      labelled exploratory and cannot promote the scorecard. Batch runs require
      a clean commit and use an isolated checkout. With no filters, this
      selects all red cells, but refuses a plan whose declared worst-case cell
      occupancy cannot fit the whole-run wall bound. Use --sample for a bounded
      random batch, --mode to narrow its population, or give --test, --mode,
      and --backend together for exactly one cell. The default result directory
      is ignored/compat-envelope/pressure-<SHA>-<time>. A red chaos cell whose
      manifest declares no seeds remains red but is unavailable: exact requests
      refuse it, while batches report and omit it rather than inventing a run.
      Add --repetitions N to repeat every selected red cell in independent boxed
      checks against the same clean committed source. Use --green with
      --repetitions to select enabled green cells instead; an exact cell, --mode,
      and --sample may narrow either population. Existing resource
      caps allow four manifest guests at once by default, including KVM guests.
      This reports per-cell flakiness; it never edits or demotes the scorecard.
      Only unfiltered --green covers the complete current green set; an exact
      cell, --mode, or --sample is partial evidence.
  plan --results DIR [--mode MODE] [--sample COUNT] [--seed SEED]
      [--cells-file PATH]
      [--green --backend BACKEND --repetitions COUNT] [--jobs COUNT]
      [--probe-disabled --backend BACKEND]
      Generate the same safe-ci execution plan without running it. The default
      output is DIR/dag.json.
  summarize --results DIR
      Re-read a completed run, print its per-backend outcome table, and rewrite
      DIR/summary.json. This never edits or promotes the checked-in scorecard.
  emit-series --results DIR
      Re-read a completed run and append its per-cell results to the parent
      series store. Requires DEV_HERMIT_PARENT and does not run a guest.
  self-test
      Test pressure-runner selection, timeout, execution-plan, and retained-
      evidence checks without running a guest.

Exact-cell options (run and plan):
  --test TEST-ID           Exact manifest test ID, such as
                           applications/example-timed-progress-bar
  --mode MODE              verify, replay, chaos, or naked
  --backend BACKEND        Backend for one exact cell, or for a
                           --probe-disabled batch
  --probe-disabled         Probe disabled cells for one backend. With --test,
                           also requires --mode; without --test, --mode may
                           narrow the disabled-backend population.
  --cell-timeout SECONDS   Maximum enclosing allowance for each selected cell;
                           refuses before launch if it cannot retain the current
                           preparation, execution, retry, and reporting bounds.
                           Requires an exact cell, --sample, or a repeated batch
  --repetitions COUNT      Repeat each selected red cell in independent boxed
                           jobs, or selected green cells with --green. COUNT must
                           be positive. Plan and run
                           require a clean commit. At most four manifest guests
                           run at once, including KVM guests.
  --run-id-prefix ID       Bind each retained result to this physical invocation.
                           Accepted only with one exact repeated cell; letters,
                           digits, '.', '_', and '-' only.

Selection and bounded-batch options (run and plan):
  --sample COUNT           Seeded random sample of red cells. Without --mode,
                           samples verify, replay, and chaos; custom and naked
                           are omitted. Sampling draws only from cells whose
                           manifests provide executable commands. With --green
                           and --repetitions, sample the enabled green cells.
  --green                  With --repetitions, select enabled green cells instead
                           of red cells. Exact --test/--mode/--backend, --mode,
                           and --sample filters are retained in run.json. A sample
                           records selected/eligible counts and its seed in
                           run.json and summary.json; it is subset evidence,
                           not a full-population result.
  --seed SEED              Reproduce one sample. If omitted, a generated seed
                           and every selected identity are retained in run.json.
  --cells-file PATH        Select exactly the canonical five-field cell JSON
                           identities of enabled executable red cells, listed
                           one per line. This is a clean-
                           commit repeated-batch selector: it requires
                           --repetitions and cannot be combined with population
                           filters. Duplicate, noncanonical, untracked,
                           unsupported, disabled, or non-executable cells are
                           rejected. run.json retains the source path, SHA-256,
                           and exact selected identities.
  --run-timeout SECONDS    Whole-run WALL-CLOCK bound (default 7200). This is
                           not a CPU budget and never weakens per-cell limits.
  --jobs COUNT             Fixed safe-ci scheduler pool (host-adaptive default).
                           The manifest-guest cap separately limits guests.
  --manifest-guest-cap N   Override the manifest_guest concurrency cap (default
                           4). Explicit caps are admitted only when the selected
                           repetitions' largest concurrent declared memory caps,
                           plus fixture preparation, fit the observed cgroup/
                           machine memory budget.
  --kvm-guest-cap N        Separately cap KVM cells (default 4). This composes
                           with --manifest-guest-cap so non-KVM work need not be
                           throttled to the KVM-safe width.

Examples:
  # Probe one cell with at most 600 seconds for its complete retry lifecycle.
  ./ci/compat-envelope/pressure-test.rs run \
    --test applications/example-timed-progress-bar \
    --mode verify --backend ptrace --cell-timeout 600

  # Reproducibly sample ten red verify/replay/chaos cells.
  ./ci/compat-envelope/pressure-test.rs run \
    --sample 10 --seed 42 --cell-timeout 600

  # Repeat every executable red verify cell twice with one shared build.
  ./ci/compat-envelope/pressure-test.rs plan \
    --results ignored/compat-envelope/repeated-red-verify \
    --mode verify --repetitions 2 --cell-timeout 600

  # Check one committed green cell 100 times under the same boxed limits.
  # The DAG admits at most four manifest guests at once.
  ./ci/compat-envelope/pressure-test.rs run \
    --test backend-parity-c/fork-exec-pipeline \
    --mode verify --backend ptrace --green \
    --repetitions 100 --cell-timeout 600

  # Check every enabled green cell once with one shared build.
  ./ci/compat-envelope/pressure-test.rs run \
    --green --repetitions 1 --run-timeout 14400

  # Inspect the bounded plan without executing it.
  ./ci/compat-envelope/pressure-test.rs plan \
    --results ignored/compat-envelope/pressure-review \
    --mode verify --sample 10 --seed 42 --cell-timeout 600

Other options:
  --results DIR            Retained ignored/ result directory
  --help                    Show this text

How it runs:
  Plan generation first checks the tracked scorecard and reads selection and
  budgets from the typed manifest tool. The in-memory graph then reuses the
  canonical Hermit/resource build commands and their submodule, pin, script,
  and manifest prerequisites from ci/dag/validate.json. It does not run the
  full validation graph. Fixture preparation is serialized. Every selected-cell
  repetition then runs in its own safe-ci cgroup. Existing resource caps admit
  four manifest guests at once, including KVM guests. A failure, timeout, OOM, or missing result does not
  intentionally stop later selected checks.
  The combined crash/error bucket contains remaining nonzero harness exits,
  including signal-caused crashes when the shell reports a nonzero status; the
  pressure runner does not currently distinguish the originating signal.
  RESULTS/dag.json is retained for inspection; execution uses this process's
  typed graph directly and never reparses that file.
"#;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CellId {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalOwnedCellId {
    backend: String,
    category: String,
    lane: String,
    mode: String,
    test: String,
}

#[derive(Debug, Deserialize)]
struct TrackedCells {
    schema: u64,
    cells: Vec<TrackedCell>,
}

fn load_tracked_cells(root: &Path) -> Result<TrackedCells, String> {
    let path = root.join(TRACKED_CELLS);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let tracked: TrackedCells = serde_json::from_str(&text)
        .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
    if tracked.schema != TRACKED_CELLS_SCHEMA {
        return Err(format!(
            "unsupported tracked cell schema {}",
            tracked.schema
        ));
    }
    Ok(tracked)
}

#[derive(Serialize)]
struct CanonicalCellId<'a> {
    backend: &'a str,
    category: &'a str,
    lane: &'a str,
    mode: &'a str,
    test: &'a str,
}

fn canonical_cell_json(cell: &CellId) -> Result<String, String> {
    serde_json::to_string(&CanonicalCellId {
        backend: &cell.backend,
        category: &cell.category,
        lane: &cell.lane,
        mode: &cell.mode,
        test: &cell.test,
    })
    .map_err(|error| format!("cannot serialize canonical cell identity: {error}"))
}

fn canonical_cells_jsonl(cells: &[CellId]) -> Result<String, String> {
    let mut text = String::new();
    for cell in cells {
        text.push_str(&canonical_cell_json(cell)?);
        text.push('\n');
    }
    Ok(text)
}

fn selected_population_sha256(cells: &[CellId]) -> Result<String, String> {
    let mut cells = cells.to_vec();
    cells.sort();
    let canonical: Vec<_> = cells
        .iter()
        .map(|cell| CanonicalCellId {
            backend: &cell.backend,
            category: &cell.category,
            lane: &cell.lane,
            mode: &cell.mode,
            test: &cell.test,
        })
        .collect();
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| format!("cannot serialize selected cell population: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn load_cells_file(path: &Path) -> Result<(Vec<CellId>, String), String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read --cells-file {}: {error}", path.display()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("--cells-file {} is not UTF-8: {error}", path.display()))?;
    if text.is_empty() {
        return Err(format!("--cells-file {} is empty", path.display()));
    }
    if !text.ends_with('\n') {
        return Err(format!(
            "--cells-file {} is not canonical JSONL: final newline is missing",
            path.display()
        ));
    }
    let mut cells = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, line) in text[..text.len() - 1].split('\n').enumerate() {
        let line_number = index + 1;
        if line.is_empty() {
            return Err(format!(
                "--cells-file {}:{line_number} is empty",
                path.display()
            ));
        }
        let parsed: CanonicalOwnedCellId = serde_json::from_str(line).map_err(|error| {
            format!(
                "--cells-file {}:{line_number} is not a five-field cell identity: {error}",
                path.display()
            )
        })?;
        let cell = CellId {
            lane: parsed.lane,
            category: parsed.category,
            test: parsed.test,
            mode: parsed.mode,
            backend: parsed.backend,
        };
        let canonical = canonical_cell_json(&cell)?;
        if line != canonical {
            return Err(format!(
                "--cells-file {}:{line_number} is not canonical JSON; expected {canonical}",
                path.display()
            ));
        }
        if !seen.insert(cell.clone()) {
            return Err(format!(
                "--cells-file {}:{line_number} repeats {}",
                path.display(),
                display_id(&cell)
            ));
        }
        cells.push(cell);
    }
    Ok((cells, digest))
}

#[derive(Clone, Debug, Deserialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    enabled: bool,
    status: String,
    /// Why a `not-applicable` cell is not applicable, verbatim from the
    /// manifest. Absent for `green` and `red`.
    #[serde(default)]
    not_applicable_reason: Option<String>,
}

struct PressureCells {
    selected: Vec<TrackedCell>,
    unavailable: Vec<TrackedCell>,
    eligible_cells: usize,
    preparation_by_test: BTreeMap<String, CellId>,
    cells_file_sha256: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct CellSelection {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    test: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    cell_timeout_seconds: Option<i64>,
    #[serde(default)]
    sample: Option<usize>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    run_timeout_seconds: Option<i64>,
    #[serde(default)]
    repetitions: Option<usize>,
    #[serde(default)]
    run_id_prefix: Option<String>,
    #[serde(default)]
    green: bool,
    #[serde(default)]
    probe_disabled: bool,
    #[serde(default)]
    jobs: Option<i64>,
    #[serde(default)]
    manifest_guest_cap: Option<i64>,
    #[serde(default)]
    kvm_guest_cap: Option<i64>,
    #[serde(default)]
    cells_file: Option<PathBuf>,
    /// Exact cells retained in run.json are sufficient to revalidate an old
    /// run without depending on the continued existence of its source file.
    #[serde(skip)]
    retained_cells_file_cells: Option<Vec<CellId>>,
}

impl CellSelection {
    fn is_exact(&self) -> bool {
        self.test.is_some() && self.mode.is_some() && self.backend.is_some()
    }

    fn repeats_cells(&self) -> bool {
        self.repetitions.is_some()
    }

    fn selects_green_population(&self) -> bool {
        self.green
    }

    fn uses_shared_preparation(&self) -> bool {
        !self.is_exact() || self.repeats_cells()
    }

    fn run_count(&self) -> usize {
        self.repetitions.unwrap_or(1)
    }

    fn scheduler_jobs(&self) -> i64 {
        self.jobs.unwrap_or_else(default_jobs)
    }

    fn manifest_guest_cap(&self) -> i64 {
        self.manifest_guest_cap
            .unwrap_or(DEFAULT_MANIFEST_GUEST_CAP)
    }

    fn kvm_guest_cap(&self) -> i64 {
        self.kvm_guest_cap.unwrap_or(DEFAULT_KVM_GUEST_CAP)
    }

    fn allows_dirty_source(&self) -> bool {
        self.is_exact() && !self.repeats_cells()
    }

    /// Repetitions of a SET of cells rather than of one named cell.
    ///
    /// Distinct from [`Self::is_exact`] with repetitions, which repeats a single
    /// `--test/--mode/--backend` cell, and from a bare batch, which probes every
    /// selected cell once. This is the shape a stability question needs: many
    /// cells, each run several times.
    fn repeats_batch(&self) -> bool {
        self.repetitions.is_some() && !self.is_exact()
    }
}

fn validate_selection_shape(selection: &CellSelection) -> Result<(), String> {
    if selection.scheduler_jobs() <= 0
        || selection.manifest_guest_cap() <= 0
        || selection.kvm_guest_cap() <= 0
    {
        return Err("pressure-test scheduler, manifest guest, and KVM caps must be positive".into());
    }
    if let Some(cap) = selection.manifest_guest_cap {
        if cap > selection.scheduler_jobs() {
            return Err(format!(
                "--manifest-guest-cap {cap} exceeds scheduler --jobs {}; a resource cap above scheduler width has no effect",
                selection.scheduler_jobs()
            ));
        }
    }
    if let Some(cap) = selection.kvm_guest_cap {
        if cap > selection.scheduler_jobs() || cap > selection.manifest_guest_cap() {
            return Err(format!(
                "--kvm-guest-cap {cap} exceeds the effective manifest/scheduler width {}; a KVM cap above it has no effect",
                selection.scheduler_jobs().min(selection.manifest_guest_cap())
            ));
        }
    }
    if selection.cells_file.is_some() || selection.retained_cells_file_cells.is_some() {
        if selection.repetitions.is_none() {
            return Err("--cells-file requires --repetitions".into());
        }
        if selection.test.is_some()
            || selection.mode.is_some()
            || selection.backend.is_some()
            || selection.sample.is_some()
            || selection.seed.is_some()
            || selection.green
            || selection.probe_disabled
            || selection.run_id_prefix.is_some()
        {
            return Err(
                "--cells-file cannot be combined with --test, --mode, --backend, --sample, --seed, --green, --probe-disabled, or --run-id-prefix"
                    .into(),
            );
        }
        if selection.cells_file.is_some() && selection.retained_cells_file_cells.is_some() {
            return Err("cell-file selection has both source and retained identities".into());
        }
    }
    if selection.probe_disabled {
        if selection.backend.is_none() {
            return Err("--probe-disabled requires --backend".into());
        }
        if selection.green {
            return Err("--probe-disabled and --green are mutually exclusive".into());
        }
        if selection.test.is_some() && selection.mode.is_none() {
            return Err("--probe-disabled with --test also requires --mode".into());
        }
    } else {
        let exact_fields = [
            selection.test.is_some(),
            selection.mode.is_some() && (selection.test.is_some() || selection.backend.is_some()),
            selection.backend.is_some(),
        ];
        if (selection.test.is_some() || !selection.green)
            && exact_fields.iter().any(|present| *present)
            && !exact_fields.iter().all(|present| *present)
        {
            return Err(
                "an exact-cell selection requires --test, --mode, and --backend together".into(),
            );
        }
    }
    Ok(())
}

fn population_label(green: bool, probe_disabled: bool) -> &'static str {
    if green {
        "green"
    } else if probe_disabled {
        "disabled"
    } else {
        "red"
    }
}

fn validate_repetition_selection(selection: &CellSelection) -> Result<(), String> {
    let Some(repetitions) = selection.repetitions else {
        if selection.run_id_prefix.is_some() {
            return Err("--run-id-prefix requires --repetitions".into());
        }
        if selection.green {
            return Err("--green requires --repetitions".into());
        }
        return Ok(());
    };
    if repetitions == 0 {
        return Err("--repetitions must be positive".into());
    }
    if selection.is_exact() {
        if selection.sample.is_some() || selection.seed.is_some() {
            return Err("an exact repeated cell cannot be combined with --sample or --seed".into());
        }
        if let Some(prefix) = &selection.run_id_prefix {
            if prefix.is_empty()
                || !prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                return Err(
                    "--run-id-prefix must contain only ASCII letters, digits, '.', '_', or '-'"
                        .into(),
                );
            }
        }
        return Ok(());
    }
    // A RED BATCH MAY REPEAT TOO, and the machinery was always generic: the plan
    // writer expands every tracked cell through `repetition_numbers`, with
    // nothing green-specific in it. Only this check stood in the way, so the
    // whole red population could be probed ONCE each or one red cell repeated,
    // and never many red cells repeated -- which is exactly the shape a
    // stability question needs. Reaching it meant one invocation per cell, an
    // ad-hoc loop around a tool that already knew how to schedule and bound the
    // work itself.
    // A backend filter is otherwise an incomplete exact-cell selector. The one
    // batch exception is explicit disabled-cell probing, where the backend is
    // required so a broad run cannot accidentally exercise every unsupported
    // implementation.
    if selection.test.is_some()
        || (selection.backend.is_some()
            && !selection.probe_disabled
            && !selection.selects_green_population())
    {
        return Err(
            "a repeated batch accepts only an optional --mode filter, unless \
             --probe-disabled names one --backend; name a full \
             --test/--mode/--backend cell to repeat exactly one"
                .into(),
        );
    }
    if selection.run_id_prefix.is_some() {
        return Err("--run-id-prefix is limited to one exact repeated cell".into());
    }
    if selection.seed.is_some() && selection.sample.is_none() {
        return Err("--seed requires --sample".into());
    }
    Ok(())
}

struct FreshCheckout {
    source: PathBuf,
    parent: PathBuf,
    canonical_parent: PathBuf,
    parent_device: u64,
    parent_inode: u64,
    path: PathBuf,
    path_device: u64,
    path_inode: u64,
    sha: String,
    marker_written: bool,
}

struct SelfTestDirectory {
    path: PathBuf,
    expected_parent: PathBuf,
    expected_prefix: String,
    armed: bool,
}

impl SelfTestDirectory {
    fn new(path: PathBuf) -> Self {
        Self::at(path, env::temp_dir(), "hermit-pressure-self-test-")
    }

    fn at(path: PathBuf, expected_parent: PathBuf, expected_prefix: &str) -> Self {
        Self {
            path,
            expected_parent,
            expected_prefix: expected_prefix.into(),
            armed: true,
        }
    }

    fn remove(mut self) -> Result<(), String> {
        fs::remove_dir_all(&self.path).map_err(|e| {
            format!(
                "cannot remove self-test directory {}: {e}",
                self.path.display()
            )
        })?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for SelfTestDirectory {
    fn drop(&mut self) {
        if self.armed
            && self.path.parent() == Some(self.expected_parent.as_path())
            && self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&self.expected_prefix))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

const LOCAL_CLONE_ARGS: [&str; 4] = ["clone", "--local", "--no-hardlinks", "--no-checkout"];

fn fresh_checkout_parent(source: &Path) -> Result<(PathBuf, PathBuf, u64, u64), String> {
    let host_tmp = fs::canonicalize("/tmp")
        .map_err(|e| format!("cannot resolve host /tmp before pressure execution: {e}"))?;
    let canonical_source = fs::canonicalize(source).map_err(|e| {
        format!(
            "cannot resolve pressure-test source checkout {}: {e}",
            source.display()
        )
    })?;
    if canonical_source.starts_with(&host_tmp) {
        return Err(format!(
            "batch pressure execution refuses source checkout {} because it is under host /tmp, which Hermit replaces for the guest; use a checkout outside /tmp",
            source.display()
        ));
    }

    let parent = source.join("ignored");
    match fs::symlink_metadata(&parent) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "batch pressure execution refuses symlinked generated-checkout parent {}",
                parent.display()
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "generated-checkout parent {} is not a directory",
                parent.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&parent).map_err(|e| {
                format!(
                    "cannot create fresh-checkout parent {}: {e}",
                    parent.display()
                )
            })?;
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect fresh-checkout parent {}: {error}",
                parent.display()
            ));
        }
    }
    let ignored = Command::new("git")
        .args([
            "-C",
            &source.to_string_lossy(),
            "check-ignore",
            "-q",
            "--",
            "ignored/",
        ])
        .status()
        .map_err(|e| {
            format!(
                "cannot verify that {} is ignored by Git: {e}",
                parent.display()
            )
        })?;
    if !ignored.success() {
        return Err(format!(
            "batch pressure execution refuses generated-checkout parent {} because Git does not ignore it",
            parent.display()
        ));
    }
    let canonical_parent = fs::canonicalize(&parent).map_err(|e| {
        format!(
            "cannot resolve fresh-checkout parent {}: {e}",
            parent.display()
        )
    })?;
    if canonical_parent.starts_with(&host_tmp) {
        return Err(format!(
            "batch pressure execution refuses generated checkout parent {} because it resolves under host /tmp, which Hermit replaces for the guest",
            parent.display()
        ));
    }
    if !canonical_parent.starts_with(&canonical_source) {
        return Err(format!(
            "batch pressure execution refuses generated-checkout parent {} because it resolves outside source checkout {}",
            parent.display(),
            source.display()
        ));
    }
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(&parent).map_err(|e| {
        format!(
            "cannot inspect generated-checkout parent {}: {e}",
            parent.display()
        )
    })?;
    Ok((parent, canonical_parent, metadata.dev(), metadata.ino()))
}

fn validate_generated_checkout_path(
    path: &Path,
    parent: &Path,
    canonical_parent: &Path,
    parent_device: u64,
    parent_inode: u64,
    expected_path_identity: Option<(u64, u64)>,
) -> Result<(u64, u64), String> {
    let name_ok = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("pressure-fresh-"));
    if path.parent() != Some(parent) || !name_ok {
        return Err(format!(
            "generated checkout has unexpected path {}",
            path.display()
        ));
    }
    use std::os::unix::fs::MetadataExt;
    let observed_parent = fs::symlink_metadata(parent).map_err(|e| {
        format!(
            "cannot inspect generated-checkout parent {}: {e}",
            parent.display()
        )
    })?;
    if observed_parent.file_type().is_symlink()
        || !observed_parent.is_dir()
        || observed_parent.dev() != parent_device
        || observed_parent.ino() != parent_inode
    {
        return Err(format!(
            "generated-checkout parent changed after selection: {}",
            parent.display()
        ));
    }
    let observed_canonical_parent = fs::canonicalize(parent).map_err(|e| {
        format!(
            "cannot resolve generated-checkout parent {}: {e}",
            parent.display()
        )
    })?;
    if observed_canonical_parent != canonical_parent {
        return Err(format!(
            "generated-checkout parent changed location after selection: {}",
            parent.display()
        ));
    }
    let observed_path = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect generated checkout {}: {e}", path.display()))?;
    if observed_path.file_type().is_symlink() || !observed_path.is_dir() {
        return Err(format!(
            "generated checkout is not a real directory: {}",
            path.display()
        ));
    }
    let canonical_path = fs::canonicalize(path)
        .map_err(|e| format!("cannot resolve generated checkout {}: {e}", path.display()))?;
    if canonical_path.parent() != Some(canonical_parent) {
        return Err(format!(
            "generated checkout {} resolves outside its recorded parent {}",
            path.display(),
            parent.display()
        ));
    }
    let observed_identity = (observed_path.dev(), observed_path.ino());
    if expected_path_identity.is_some_and(|expected| expected != observed_identity) {
        return Err(format!(
            "generated checkout changed after creation: {}",
            path.display()
        ));
    }
    Ok(observed_identity)
}

fn clone_local_without_hardlinks(source: &Path, destination: &Path) -> Result<(), String> {
    command_ok(
        Command::new("git")
            .args(LOCAL_CLONE_ARGS)
            .arg(source)
            .arg(destination),
        "materialize fresh pressure-test checkout",
    )
}

impl FreshCheckout {
    fn prepare(source: &Path, sha: &str) -> Result<Self, String> {
        let (parent, canonical_parent, parent_device, parent_inode) =
            fresh_checkout_parent(source)?;
        let template = parent.join("pressure-fresh-XXXXXXXX");
        let output = Command::new("mktemp")
            .args(["-d", &template.to_string_lossy()])
            .output()
            .map_err(|e| format!("cannot create fresh checkout: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "mktemp refused fresh checkout creation: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let (path_device, path_inode) = validate_generated_checkout_path(
            &path,
            &parent,
            &canonical_parent,
            parent_device,
            parent_inode,
            None,
        )?;
        let mut checkout = Self {
            source: source.to_path_buf(),
            parent,
            canonical_parent,
            parent_device,
            parent_inode,
            path,
            path_device,
            path_inode,
            sha: sha.to_string(),
            marker_written: false,
        };
        let initialize = (|| {
            clone_local_without_hardlinks(source, &checkout.path)?;
            let marker = checkout
                .path
                .join(".git")
                .join("pressure-test-generated-checkout");
            fs::write(
                &marker,
                format!("source={}\nsha={}\n", source.display(), sha),
            )
            .map_err(|e| format!("cannot write {}: {e}", marker.display()))?;
            checkout.marker_written = true;
            command_ok(
                Command::new("git")
                    .args([
                        "-C",
                        &checkout.path.to_string_lossy(),
                        "checkout",
                        "--detach",
                    ])
                    .arg(sha),
                "check out exact pressure-test commit",
            )?;
            let observed = git_output(&checkout.path, &["rev-parse", "HEAD"])?;
            if observed != sha {
                return Err(format!(
                    "fresh pressure-test checkout resolved to {observed}, expected {sha}"
                ));
            }
            command_ok(
                Command::new("git").args([
                    "-C",
                    &checkout.path.to_string_lossy(),
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                ]),
                "initialize pressure-test submodules",
            )?;
            for required in [
                "ci/compat-envelope/pressure-test.rs",
                "agent-utils/rs/dagrun/Cargo.toml",
            ] {
                if !checkout.path.join(required).is_file() {
                    return Err(format!(
                        "fresh pressure-test checkout is missing required file {required}"
                    ));
                }
            }
            Ok(())
        })();
        if let Err(error) = initialize {
            let cleanup = checkout.cleanup();
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; fresh-checkout cleanup also failed: {cleanup}"),
            });
        }
        Ok(checkout)
    }

    fn cleanup(&self) -> Result<(), String> {
        validate_generated_checkout_path(
            &self.path,
            &self.parent,
            &self.canonical_parent,
            self.parent_device,
            self.parent_inode,
            Some((self.path_device, self.path_inode)),
        )
        .map_err(|error| {
            format!(
                "refusing to remove generated checkout {}: {error}",
                self.path.display()
            )
        })?;
        let marker = self
            .path
            .join(".git")
            .join("pressure-test-generated-checkout");
        let expected_marker = format!("source={}\nsha={}\n", self.source.display(), self.sha);
        match fs::read_to_string(&marker) {
            Ok(observed_marker) if observed_marker == expected_marker => {}
            Ok(_) => {
                return Err(format!(
                    "refusing cleanup because {} does not match this run",
                    marker.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !self.marker_written => {
                // Initialization may fail before the marker is written. This
                // object still owns the freshly minted, parent/name-checked
                // directory, so refusing here would leak every failed clone.
            }
            Err(error) => {
                return Err(format!(
                    "refusing cleanup without readable {}: {error}",
                    marker.display()
                ));
            }
        }
        fs::remove_dir_all(&self.path)
            .map_err(|e| format!("cannot remove generated clone {}: {e}", self.path.display()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CellBudget {
    cpu_timeout_seconds: i64,
    timeout_seconds: i64,
    attempts: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ManifestBudgetRow {
    test: String,
    mode: String,
    backend: String,
    cpu_timeout_seconds: i64,
    timeout_seconds: i64,
    attempts: JsonValue,
}

/// Additive run metadata distinguishes the current producer contract from
/// historical campaigns. Keep the independent multipliers used to generate the
/// plan, so a later reader's environment cannot reinterpret its recorded caps.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PressureTimeoutPolicy {
    version: u64,
    cpu_multiplier: f64,
    wall_multiplier: f64,
}

impl PressureTimeoutPolicy {
    fn from_env() -> Result<Self, String> {
        let multipliers = timeout_multipliers_from_env()?;
        Ok(Self {
            version: 1,
            cpu_multiplier: multipliers.cpu,
            wall_multiplier: multipliers.wall,
        })
    }

    fn multipliers(&self) -> Result<TimeoutMultipliers, String> {
        if self.version != 1 {
            return Err(format!("unsupported pressure timeout policy {}", self.version));
        }
        Ok(TimeoutMultipliers {
            cpu: validate_timeout_multiplier(self.cpu_multiplier, "recorded CPU multiplier")?,
            wall: validate_timeout_multiplier(self.wall_multiplier, "recorded wall multiplier")?,
        })
    }
}

fn deserialize_timeout_policy<'de, D>(deserializer: D) -> Result<Option<PressureTimeoutPolicy>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let policy = PressureTimeoutPolicy::deserialize(deserializer)?;
    policy.multipliers().map_err(serde::de::Error::custom)?;
    Ok(Some(policy))
}

fn current_result_policy(metadata: &RunMetadata, fresh: bool) -> Result<bool, String> {
    if let Some(policy) = metadata.timeout_policy {
        policy.multipliers()?;
        Ok(true)
    } else if fresh {
        Err("fresh pressure run omitted its timeout policy".into())
    } else {
        Ok(false)
    }
}

fn resolve_budgets(
    mut budgets: BTreeMap<(String, String, String), CellBudget>,
    policy: PressureTimeoutPolicy,
    selected: &BTreeSet<(String, String, String)>,
) -> Result<BTreeMap<(String, String, String), CellBudget>, String> {
    let multipliers = policy.multipliers()?;
    budgets.retain(|key, _| selected.contains(key));
    for budget in budgets.values_mut() {
        let resolved = resolve_test_timeouts(
            u64::try_from(budget.cpu_timeout_seconds).map_err(|_| "negative CPU timeout")?,
            u64::try_from(budget.timeout_seconds).map_err(|_| "negative wall timeout")?,
            multipliers,
        )?;
        budget.cpu_timeout_seconds = i64::try_from(resolved.cpu_seconds)
            .map_err(|_| "resolved CPU timeout exceeds the supported integer range")?;
        budget.timeout_seconds = i64::try_from(resolved.wall_seconds)
            .map_err(|_| "resolved wall timeout exceeds the supported integer range")?;
    }
    Ok(budgets)
}

fn read_current_result_rows(path: &Path) -> Result<Vec<CellResult>, String> {
    let rows = read_result_rows(path)?;
    for row in &rows {
        row.require_current_timeout_policy().map_err(|error| {
            format!("{} attempt {} has invalid current timeout evidence: {error}", path.display(), row.attempt)
        })?;
    }
    Ok(rows)
}

fn read_result_rows(path: &Path) -> Result<Vec<CellResult>, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut rows: Vec<CellResult> = Vec::new();
    let mut attempts = BTreeSet::new();
    let mut artifact_dirs = BTreeSet::new();
    let mut previous_attempt = None;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: CellResult = serde_json::from_str(line)
            .map_err(|e| format!("invalid {}:{}: {e}", path.display(), index + 1))?;
        row.validate_recorded_classification().map_err(|error| {
            format!(
                "invalid {}:{} result classification: {error}",
                path.display(),
                index + 1
            )
        })?;
        if row.attempt == 0 {
            return Err(format!(
                "{}:{} has non-positive result attempt 0",
                path.display(),
                index + 1
            ));
        }
        let expected_attempt = previous_attempt.map_or(1, |previous| previous + 1);
        if row.attempt != expected_attempt {
            return Err(format!(
                "{}:{} result attempt {} does not follow the preceding attempts; expected {}",
                path.display(),
                index + 1,
                row.attempt,
                expected_attempt
            ));
        }
        if !attempts.insert(row.attempt) {
            return Err(format!(
                "{} contains duplicate result attempt {}",
                path.display(),
                row.attempt
            ));
        }
        if row.attempt > 1 && row.timeout_seconds == 0 {
            return Err(format!(
                "{}:{} retry attempt {} has no wall-clock bound",
                path.display(),
                index + 1,
                row.attempt
            ));
        }
        if row.artifact_dir.is_empty() || !artifact_dirs.insert(row.artifact_dir.clone()) {
            return Err(format!(
                "{}:{} result attempt {} has an empty or reused artifact directory",
                path.display(),
                index + 1,
                row.attempt
            ));
        }
        if row.schema != CELL_RESULT_SCHEMA {
            return Err(format!(
                "{}:{} has unsupported cell-result schema {}",
                path.display(),
                index + 1,
                row.schema
            ));
        }
        if let Some(first) = rows.first() {
            if row.run_id != first.run_id
                || row.hermit_sha != first.hermit_sha
                || row.source_tree_dirty != first.source_tree_dirty
                || row.test != first.test
                || row.category != first.category
                || row.lane != first.lane
                || row.mode != first.mode
                || row.backend != first.backend
                || row.classification != first.classification
            {
                return Err(format!(
                    "{}:{} mixes a different cell identity into one result file",
                    path.display(),
                    index + 1
                ));
            }
        }
        previous_attempt = Some(row.attempt);
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(format!("{} contains no result rows", path.display()));
    }
    if rows.len() > 1 && rows[0].timeout_seconds == 0 {
        return Err(format!(
            "{}:1 retry history attempt 1 has no wall-clock bound",
            path.display()
        ));
    }
    Ok(rows)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunMetadata {
    schema: u64,
    #[serde(default)]
    run_id: String,
    hermit_sha: String,
    detcore_tree: String,
    source_tree_dirty: bool,
    run_timeout_seconds: i64,
    #[serde(default, deserialize_with = "deserialize_timeout_policy", skip_serializing_if = "Option::is_none")]
    timeout_policy: Option<PressureTimeoutPolicy>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    test: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    cell_timeout_seconds: Option<i64>,
    #[serde(default)]
    sample: Option<usize>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    unavailable_cells: usize,
    #[serde(default)]
    repetitions: Option<usize>,
    #[serde(default)]
    run_id_prefix: Option<String>,
    #[serde(default)]
    green: bool,
    #[serde(default)]
    probe_disabled: bool,
    #[serde(default = "default_pressure_jobs")]
    jobs: i64,
    #[serde(default = "default_manifest_guest_cap")]
    manifest_guest_cap: i64,
    #[serde(default)]
    manifest_guest_cap_explicit: bool,
    #[serde(default = "default_kvm_guest_cap")]
    kvm_guest_cap: i64,
    #[serde(default)]
    kvm_guest_cap_explicit: bool,
    #[serde(default)]
    manifest_guest_memory_budget_bytes: Option<i64>,
    #[serde(default)]
    manifest_guest_memory_required_bytes: Option<i64>,
    #[serde(default)]
    manifest_guest_control_plane_headroom_bytes: Option<i64>,
    #[serde(default)]
    manifest_guest_max_safe_cap: Option<i64>,
    #[serde(default)]
    kvm_guest_max_safe_cap: Option<i64>,
    #[serde(default)]
    eligible_cells: usize,
    #[serde(default)]
    cells_file: Option<String>,
    #[serde(default)]
    cells_file_sha256: Option<String>,
    #[serde(default)]
    selected_population_sha256: Option<String>,
    cells: Vec<CellId>,
}

impl RunMetadata {
    fn is_exact(&self) -> bool {
        self.test.is_some() && self.mode.is_some() && self.backend.is_some()
    }
}

fn default_pressure_jobs() -> i64 {
    default_jobs()
}

fn default_manifest_guest_cap() -> i64 {
    DEFAULT_MANIFEST_GUEST_CAP
}

fn default_kvm_guest_cap() -> i64 {
    DEFAULT_KVM_GUEST_CAP
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RunnerEvidence {
    seen: bool,
    ok: bool,
    timed_out: bool,
    oom: bool,
    output_log_available: bool,
    environmental_block_observation: EnvBlockObservation,
}

impl Default for RunnerEvidence {
    fn default() -> Self {
        Self {
            seen: false,
            ok: false,
            timed_out: false,
            oom: false,
            output_log_available: false,
            environmental_block_observation: EnvBlockObservation::NothingObserved,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedOutcome {
    tag: String,
    ok: bool,
    duration_s: f64,
    returncode: Option<i64>,
    oomed: bool,
    oom_kills: i64,
    timed_out: bool,
    cpu_timed_out: bool,
    reason: String,
    aborted: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedExecution {
    schema: u64,
    scheduler_passes: usize,
    outcomes: Vec<RetainedOutcome>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedOutcomeV1 {
    tag: String,
    ok: bool,
    duration_s: f64,
    returncode: Option<i64>,
    reason: String,
    aborted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedExecutionV1 {
    schema: u64,
    scheduler_passes: usize,
    outcomes: Vec<RetainedOutcomeV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedExecutionV3 {
    schema: u64,
    scheduler_passes: usize,
    outcomes: Vec<JsonValue>,
}

struct ExecutionEvidence {
    outcomes: Vec<StepOutcome>,
    passes: usize,
    scheduler_wall_s: f64,
    step_profile_rows: Vec<BTreeMap<String, String>>,
}

fn outcome_evidence(outcome: &StepOutcome) -> RunnerEvidence {
    RunnerEvidence {
        seen: true,
        ok: outcome.ok,
        timed_out: outcome.timed_out || outcome.cpu_timed_out,
        oom: outcome.oomed,
        ..RunnerEvidence::default()
    }
}

fn retained_outcome_evidence(outcome: &RetainedOutcome) -> Result<RunnerEvidence, String> {
    if outcome.oom_kills < 0 || outcome.oomed != (outcome.oom_kills > 0) {
        return Err(format!(
            "typed scheduler outcome {} disagrees about oomed={} and oom_kills={}",
            outcome.tag, outcome.oomed, outcome.oom_kills
        ));
    }
    if outcome.ok && (outcome.oomed || outcome.timed_out || outcome.cpu_timed_out) {
        return Err(format!(
            "typed scheduler outcome {} is both successful and terminated by a resource bound",
            outcome.tag
        ));
    }
    Ok(RunnerEvidence {
        seen: true,
        ok: outcome.ok,
        timed_out: outcome.timed_out || outcome.cpu_timed_out,
        oom: outcome.oomed,
        ..RunnerEvidence::default()
    })
}

fn retained_outcome_v1_evidence(outcome: &RetainedOutcomeV1) -> RunnerEvidence {
    // Schema 1 predates typed termination facts. Keep that exact historical
    // interpretation readable; only schema 2 can establish the current typed
    // contract, and there is no schema-1 write path.
    let reason = outcome.reason.to_ascii_uppercase();
    RunnerEvidence {
        seen: true,
        ok: outcome.ok,
        timed_out: reason.contains("TIMEOUT"),
        oom: reason.contains("OOM-KILLED"),
        ..RunnerEvidence::default()
    }
}

fn outcome_evidence_with_output(
    outcome: &StepOutcome,
    output_log_available: bool,
    environmental_block_observation: EnvBlockObservation,
) -> RunnerEvidence {
    RunnerEvidence {
        output_log_available,
        environmental_block_observation,
        ..outcome_evidence(outcome)
    }
}

fn execute_typed_dag(
    dag: &DagConfig,
    jobs: i64,
    cgroups: BoxedCgroups,
    started: Instant,
    run_timeout_seconds: i64,
) -> Result<ExecutionEvidence, String> {
    let expected: BTreeSet<String> = dag.steps.iter().map(Step::tag).collect();
    if expected.len() != dag.steps.len() {
        return Err("typed pressure graph contains duplicate step identities".into());
    }
    let mut completed = BTreeMap::<String, StepOutcome>::new();
    let mut passes = 0usize;
    let mut scheduler_wall_s = 0.0_f64;
    let mut step_profile_rows = Vec::new();

    while completed.len() < expected.len() {
        let remaining = run_timeout_seconds.saturating_sub(started.elapsed().as_secs() as i64);
        if remaining <= 0 {
            return Err(format!(
                "pressure run reached its {run_timeout_seconds}s whole-run bound"
            ));
        }
        let mut pass = dag.clone();
        pass.steps
            .retain(|step| !completed.contains_key(&step.tag()));
        for step in &mut pass.steps {
            step.deps
                .retain(|dependency| !completed.contains_key(dependency));
        }
        if pass.steps.is_empty() {
            return Err(
                "typed pressure graph has unfinished identities but no runnable pass".into(),
            );
        }

        passes += 1;
        // This graph clones the canonical build steps out of ci/dag/validate.json,
        // including the two that bake a 32-wide cargo invocation into the command and
        // therefore carry an empty jobs_flag. The runner refuses before any node
        // starts if the CPU budget is narrower than such a step's declared width, and
        // the budget defaults to `jobs`, so it must be passed explicitly here for the
        // same reason as in scripts/validate.rs::scheduler_cpu_budget.
        let cpu_budget = container_core_budget().min(aggregate_slice_max_cpus()).max(1);
        let result: RunResult = run_dag_boxed_deadline(
            &pass,
            jobs,
            true,
            1,
            cgroups.clone(),
            None,
            Some(cpu_budget),
            Some(remaining),
        );
        if result.run_timed_out {
            return Err(format!(
                "pressure run reached its {run_timeout_seconds}s whole-run bound during scheduler pass {passes}"
            ));
        }
        scheduler_wall_s += result.wall_s;
        step_profile_rows.extend(result.step_profile_rows.iter().cloned());

        let mut progress = 0usize;
        for outcome in result.outcomes {
            if !expected.contains(&outcome.tag) {
                return Err(format!(
                    "scheduler returned foreign step identity {}",
                    outcome.tag
                ));
            }
            if outcome.aborted {
                continue;
            }
            if completed
                .insert(outcome.tag.clone(), outcome.clone())
                .is_some()
            {
                return Err(format!(
                    "scheduler returned duplicate terminal step identity {}",
                    outcome.tag
                ));
            }
            progress += 1;
            if !outcome.ok && !outcome.tag.starts_with("cell.") {
                return Err(format!(
                    "pressure setup node {} failed: {}",
                    outcome.tag, outcome.reason
                ));
            }
        }
        if progress == 0 {
            return Err(format!(
                "scheduler pass {passes} made no terminal progress; skipped={} remaining={}",
                result.skipped.len(),
                expected.len().saturating_sub(completed.len())
            ));
        }
    }

    Ok(ExecutionEvidence {
        outcomes: expected
            .iter()
            .filter_map(|tag| completed.remove(tag))
            .collect(),
        passes,
        scheduler_wall_s,
        step_profile_rows,
    })
}

fn with_runner_log_dir<T>(
    results: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    if env::var(RUNNER_NO_LOGS_ENV).ok().as_deref() == Some("1") {
        return Err(format!(
            "{RUNNER_NO_LOGS_ENV}=1 disables retained pressure-runner evidence"
        ));
    }
    let directory = results.join("runner-profile");
    let previous = env::var_os(RUNNER_LOG_DIR_ENV);
    env::set_var(RUNNER_LOG_DIR_ENV, &directory);
    let result = action();
    match previous {
        Some(value) => env::set_var(RUNNER_LOG_DIR_ENV, value),
        None => env::remove_var(RUNNER_LOG_DIR_ENV),
    }
    if result.is_ok() && !directory.join("journal.jsonl").is_file() {
        return Err(format!(
            "typed scheduler completed without retained runner journal {}",
            directory.join("journal.jsonl").display()
        ));
    }
    result
}

fn retain_execution_evidence(
    results: &Path,
    execution: &ExecutionEvidence,
) -> Result<BTreeMap<String, RunnerEvidence>, String> {
    let output_root = results.join(RUNNER_STEP_OUTPUT_DIR);
    let mut retained = Vec::with_capacity(execution.outcomes.len());
    let mut environmental_block_observations = Vec::with_capacity(execution.outcomes.len());
    for outcome in &execution.outcomes {
        let output_log = output_root.join(format!("{}.log", sanitize_step_tag(&outcome.tag)));
        let output = fs::read_to_string(&output_log).map_err(|error| {
            format!(
                "typed scheduler discarded stdout/stderr for {}: cannot read {}: {error}",
                outcome.tag,
                output_log.display()
            )
        })?;
        environmental_block_observations.push(environmental_block_observation(&output));
        let mut retained_outcome = serde_json::to_value(RetainedOutcome {
            tag: outcome.tag.clone(),
            ok: outcome.ok,
            duration_s: outcome.duration_s,
            returncode: outcome.returncode,
            oomed: outcome.oomed,
            oom_kills: outcome.oom_kills,
            timed_out: outcome.timed_out,
            cpu_timed_out: outcome.cpu_timed_out,
            reason: outcome.reason.clone(),
            aborted: outcome.aborted,
        }).map_err(|error| format!("cannot serialize typed scheduler outcome: {error}"))?;
        retained_outcome["output_log"] = json!(output_log
            .strip_prefix(results)
            .expect("runner output is below results")
            .to_string_lossy());
        retained.push(retained_outcome);
    }
    let document = json!({
        "schema": 3,
        "scheduler_passes": execution.passes,
        "outcomes": retained,
    });
    let mut text = serde_json::to_string_pretty(&document)
        .map_err(|error| format!("cannot serialize typed scheduler outcomes: {error}"))?;
    text.push('\n');
    fs::write(results.join("runner-outcomes.json"), text)
        .map_err(|error| format!("cannot retain typed scheduler outcomes: {error}"))?;

    let profile = json!({
        "schema": 1,
        "scheduler_passes": execution.passes,
        "scheduler_wall_s": execution.scheduler_wall_s,
        "step_profile_rows": execution.step_profile_rows,
    });
    let mut profile_text = serde_json::to_string_pretty(&profile)
        .map_err(|error| format!("cannot serialize scheduler profile: {error}"))?;
    profile_text.push('\n');
    fs::write(results.join("runner-profile.json"), profile_text)
        .map_err(|error| format!("cannot retain scheduler profile: {error}"))?;

    let mut evidence = BTreeMap::new();
    for (outcome, environmental_block_observation) in execution
        .outcomes
        .iter()
        .zip(environmental_block_observations)
    {
        if outcome.tag.starts_with("cell.")
            && evidence
                .insert(
                    outcome.tag.clone(),
                    outcome_evidence_with_output(outcome, true, environmental_block_observation),
                )
                .is_some()
        {
            return Err(format!("duplicate typed cell outcome {}", outcome.tag));
        }
    }
    Ok(evidence)
}

fn load_retained_runner_evidence(
    results: &Path,
) -> Result<Option<BTreeMap<String, RunnerEvidence>>, String> {
    let path = results.join("runner-outcomes.json");
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let value: JsonValue = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    let schema = value
        .get("schema")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| format!("invalid {}: missing integer schema", path.display()))?;
    let mut evidence = BTreeMap::new();
    match schema {
        1 => {
            let retained: RetainedExecutionV1 = serde_json::from_str(&text)
                .map_err(|error| format!("invalid historical {}: {error}", path.display()))?;
            debug_assert_eq!(retained.schema, 1);
            let _ = retained.scheduler_passes;
            for outcome in retained.outcomes {
                if outcome.aborted {
                    return Err(format!(
                        "historical scheduler evidence retained aborted outcome {} as terminal",
                        outcome.tag
                    ));
                }
                if !outcome.tag.starts_with("cell.") {
                    continue;
                }
                let _ = (outcome.duration_s, outcome.returncode);
                let row = retained_outcome_v1_evidence(&outcome);
                if evidence.insert(outcome.tag.clone(), row).is_some() {
                    return Err(format!(
                        "historical scheduler evidence contains duplicate outcome {}",
                        outcome.tag
                    ));
                }
            }
        }
        2 => {
            let retained: RetainedExecution = serde_json::from_str(&text)
                .map_err(|error| format!("invalid current {}: {error}", path.display()))?;
            debug_assert_eq!(retained.schema, 2);
            let _ = retained.scheduler_passes;
            for outcome in retained.outcomes {
                if outcome.aborted {
                    return Err(format!(
                        "typed scheduler evidence retained aborted outcome {} as terminal",
                        outcome.tag
                    ));
                }
                if !outcome.tag.starts_with("cell.") {
                    continue;
                }
                let _ = (
                    outcome.duration_s,
                    outcome.returncode,
                    outcome.reason.as_str(),
                );
                let row = retained_outcome_evidence(&outcome)?;
                if evidence.insert(outcome.tag.clone(), row).is_some() {
                    return Err(format!(
                        "typed scheduler evidence contains duplicate outcome {}",
                        outcome.tag
                    ));
                }
            }
        }
        3 => {
            let retained: RetainedExecutionV3 = serde_json::from_str(&text)
                .map_err(|error| format!("invalid output-bearing {}: {error}", path.display()))?;
            debug_assert_eq!(retained.schema, 3);
            let _ = retained.scheduler_passes;
            for mut value in retained.outcomes {
                let output_log = value.as_object_mut()
                    .and_then(|row| row.remove("output_log"))
                    .and_then(|path| path.as_str().map(str::to_owned))
                    .ok_or("typed scheduler evidence requires a string output_log")?;
                let outcome: RetainedOutcome = serde_json::from_value(value)
                    .map_err(|error| format!("invalid output-bearing outcome: {error}"))?;
                if outcome.aborted {
                    return Err(format!("typed scheduler evidence retained aborted outcome {} as terminal", outcome.tag));
                }
                let mut row = retained_outcome_evidence(&outcome)?;
                let expected_log = PathBuf::from(RUNNER_STEP_OUTPUT_DIR)
                    .join(format!("{}.log", sanitize_step_tag(&outcome.tag)));
                if Path::new(&output_log) != expected_log {
                    return Err(format!("typed scheduler evidence names unexpected output log for {}: {}", outcome.tag, output_log));
                }
                let output_path = results.join(&output_log);
                let output = fs::read_to_string(&output_path).map_err(|error| {
                    format!("typed scheduler evidence lost stdout/stderr for {} at {}: {error}", outcome.tag, output_path.display())
                })?;
                row.output_log_available = true;
                row.environmental_block_observation = environmental_block_observation(&output);
                if !outcome.tag.starts_with("cell.") {
                    continue;
                }
                if evidence.insert(outcome.tag.clone(), row).is_some() {
                    return Err(format!("typed scheduler evidence contains duplicate outcome {}", outcome.tag));
                }
            }
        }
        _ => {
            return Err(format!(
                "unsupported typed scheduler outcome schema {schema}"
            ));
        }
    }
    Ok(Some(evidence))
}

fn runner_output_log(step_tag: &str, available: bool) -> Option<PathBuf> {
    available.then(|| {
        PathBuf::from(RUNNER_STEP_OUTPUT_DIR)
            .join(format!("{}.log", sanitize_step_tag(step_tag)))
    })
}

fn with_execution_root<T>(
    root: &Path,
    action: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let previous =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    env::set_current_dir(root)
        .map_err(|error| format!("cannot enter execution root {}: {error}", root.display()))?;
    let result = action();
    let restore = env::set_current_dir(&previous).map_err(|error| {
        format!(
            "cannot restore current directory {}: {error}",
            previous.display()
        )
    });
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(format!("{error}; {restore}")),
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("compatibility pressure test: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).peekable();
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{USAGE}"));
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args
        .peek()
        .is_some_and(|argument| matches!(argument.as_str(), "-h" | "--help" | "help"))
    {
        args.next();
        if args.next().is_some() {
            return Err("help accepts no additional options".into());
        }
        print!("{USAGE}");
        return Ok(());
    }
    let root = repo_root()?;
    match command.as_str() {
        "plan" => {
            let (results, output, selection) = result_options(&root, &mut args, false, true)?;
            if output.is_some() {
                return Err(
                    "plan does not accept --output; its retained and executed plan is always RESULTS/dag.json"
                        .into(),
                );
            }
            if !selection.allows_dirty_source() && worktree_dirty(&root)? {
                return Err(
                    "plan refuses a dirty checkout except for one exact cell; commit first so every batch or repeated check binds to one source commit"
                        .into(),
                );
            }
            require_empty_result_dir(&results)?;
            let output = results.join("dag.json");
            let (metadata, dag) = write_plan(&root, &results, &output, &selection)?;
            println!("DAG: {}", output.display());
            println!("Results: {}", results.display());
            println!(
                "Cell runs: {}",
                metadata
                    .cells
                    .len()
                    .saturating_mul(metadata.repetitions.unwrap_or(1))
            );
            print_manifest_guest_memory(&metadata);
            print_unavailable(&metadata);
            println!("Whole-run bound: {}s", metadata.run_timeout_seconds);
            print_sample(&metadata);
            if selection.is_exact() {
                print_exact_manifest_command(&dag, &metadata)?;
            }
            println!(
                "Inspection only: `run` builds the same typed graph in memory; dag.json is never execution authority."
            );
        }
        "run" => {
            let (results, output, selection) = result_options(&root, &mut args, true, true)?;
            if output.is_some() {
                return Err(
                    "run does not accept --output; its plan is always RESULTS/dag.json".into(),
                );
            }
            let exact_cell = selection.is_exact();
            if !selection.allows_dirty_source() && worktree_dirty(&root)? {
                return Err("run refuses a dirty checkout; commit first so every row binds to reproducible source".into());
            }
            let run_timeout_seconds = selection
                .run_timeout_seconds
                .unwrap_or(PRESSURE_RUN_TIMEOUT_SECONDS);
            let cgroups = establish_pressure_cgroups(run_timeout_seconds)?;
            require_empty_result_dir(&results)?;
            let started = Instant::now();
            let sha = git_output(&root, &["rev-parse", "HEAD"])?;
            let fresh = if selection.allows_dirty_source() {
                eprintln!(
                    "compatibility pressure test: exact-cell iteration uses the current working tree; dirty results are exploratory and cannot promote the scorecard"
                );
                None
            } else {
                let fresh = FreshCheckout::prepare(&root, &sha)?;
                println!("Fresh checkout: {}", fresh.path.display());
                Some(fresh)
            };
            let execution_root = fresh
                .as_ref()
                .map(|checkout| checkout.path.as_path())
                .unwrap_or(root.as_path());
            let output = results.join("dag.json");
            let run_result = (|| {
                let (metadata, dag) = write_plan(execution_root, &results, &output, &selection)?;
                print_manifest_guest_memory(&metadata);
                print_unavailable(&metadata);
                print_sample(&metadata);
                if exact_cell {
                    print_exact_manifest_command(&dag, &metadata)?;
                }
                let execution = with_runner_log_dir(&results, || {
                    with_execution_root(execution_root, || {
                        execute_typed_dag(
                            &dag,
                            metadata.jobs,
                            cgroups.clone(),
                            started,
                            metadata.run_timeout_seconds,
                        )
                    })
                })?;
                let runner_evidence = retain_execution_evidence(&results, &execution)?;
                let expected_runs = metadata
                    .cells
                    .len()
                    .saturating_mul(metadata.repetitions.unwrap_or(1));
                if runner_evidence.len() != expected_runs {
                    return Err(format!(
                        "typed scheduler returned {} cell outcomes, expected exactly {expected_runs}",
                        runner_evidence.len()
                    ));
                }
                println!(
                    "Scheduler: {} pass(es), fixed -j {}, {:.3}s scheduler wall",
                    execution.passes, metadata.jobs, execution.scheduler_wall_s
                );
                summarize(
                    execution_root,
                    &results,
                    selection.allows_dirty_source(),
                    Some(&runner_evidence),
                    true,
                )?;
                Ok(())
            })();
            let series_result = if std::env::var_os("DEV_HERMIT_PARENT").is_some() {
                emit_series(&results, execution_root, true)
            } else {
                Ok(())
            };
            let run_result = match (run_result, series_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(run), Ok(())) => Err(run),
                (Ok(()), Err(series)) => Err(series),
                (Err(run), Err(series)) => Err(format!(
                    "{run}; completed cell results also failed to emit: {series}"
                )),
            };
            let cleanup_result = match fresh {
                Some(fresh) => fresh.cleanup(),
                None => Ok(()),
            };
            match (run_result, cleanup_result) {
                (Ok(()), Ok(())) => {}
                (Err(run), Ok(())) => return Err(run),
                (Ok(()), Err(cleanup)) => return Err(cleanup),
                (Err(run), Err(cleanup)) => {
                    return Err(format!(
                        "{run}; fresh-checkout cleanup also failed: {cleanup}"
                    ));
                }
            }
        }
        "summarize" => {
            let (results, output, _) = result_options(&root, &mut args, false, false)?;
            if output.is_some() {
                return Err("summarize does not accept --output".into());
            }
            // Dirty retained results are admissible only when their own
            // metadata proves they came from one exact cell; summarize()
            // enforces that boundary before reading any evidence.
            summarize(&root, &results, true, None, false)?;
        }
        "emit-series" => {
            let (results, output, _) = result_options(&root, &mut args, false, false)?;
            if output.is_some() {
                return Err("emit-series does not accept --output".into());
            }
            emit_series(&results, &root, false)?;
        }
        "self-test" => {
            if args.next().is_some() {
                return Err("self-test accepts no options".into());
            }
            self_test(&root)?;
        }
        _ => return Err(format!("unknown command `{command}`\n\n{USAGE}")),
    }
    Ok(())
}

fn print_sample(metadata: &RunMetadata) {
    let Some(count) = metadata.sample else {
        return;
    };
    if metadata.eligible_cells == 0 {
        println!(
            "Sample: selected {count} cell(s), eligible count not retained by this older run, seed {}",
            metadata.seed.unwrap_or(0)
        );
    } else {
        println!(
            "Sample: selected {count} of {} eligible cell(s), seed {}",
            metadata.eligible_cells,
            metadata.seed.unwrap_or(0)
        );
    }
    for cell in &metadata.cells {
        println!("  {}", display_id(cell));
    }
}

fn print_manifest_guest_memory(metadata: &RunMetadata) {
    let budget = metadata
        .manifest_guest_memory_budget_bytes
        .map_or_else(|| "unknown".into(), |bytes| bytes.to_string());
    let required = metadata
        .manifest_guest_memory_required_bytes
        .map_or_else(|| "unknown".into(), |bytes| bytes.to_string());
    let max_safe = metadata
        .manifest_guest_max_safe_cap
        .map_or_else(|| "unknown".into(), |cap| cap.to_string());
    let max_safe_kvm = metadata
        .kvm_guest_max_safe_cap
        .map_or_else(|| "unknown".into(), |cap| cap.to_string());
    println!(
        "Manifest guests: cap {} (KVM cap {}), declared peak {} bytes including retained {}-byte control headroom; observed budget {} bytes; highest safe caps at -j {}: manifest={}, KVM={}",
        metadata.manifest_guest_cap,
        metadata.kvm_guest_cap,
        required,
        metadata
            .manifest_guest_control_plane_headroom_bytes
            .unwrap_or(CONTROL_PLANE_HEADROOM_BYTES),
        budget,
        metadata.jobs,
        max_safe,
        max_safe_kvm,
    );
}

fn print_unavailable(metadata: &RunMetadata) {
    if metadata.unavailable_cells > 0 {
        let population = population_label(metadata.green, metadata.probe_disabled);
        println!(
            "Unavailable {population} cells omitted: {} (their manifests declare no executable attempts)",
            metadata.unavailable_cells
        );
    }
}

fn result_options(
    root: &Path,
    args: &mut impl Iterator<Item = String>,
    default_results: bool,
    allow_selection: bool,
) -> Result<(PathBuf, Option<PathBuf>, CellSelection), String> {
    let mut results = None;
    let mut output = None;
    let mut selection = CellSelection::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--results" => {
                results = Some(PathBuf::from(
                    args.next().ok_or("--results requires a directory")?,
                ));
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a file")?,
                ));
            }
            "--mode" if allow_selection => {
                let value = args.next().ok_or("--mode requires a value")?;
                if !matches!(value.as_str(), "verify" | "replay" | "chaos" | "naked") {
                    return Err(format!(
                        "unknown mode `{value}`; expected verify, replay, chaos, or naked"
                    ));
                }
                if selection.mode.replace(value).is_some() {
                    return Err("--mode may be specified only once".into());
                }
            }
            "--test" if allow_selection => {
                let value = args.next().ok_or("--test requires a manifest test ID")?;
                if value.is_empty() {
                    return Err("--test requires a nonempty manifest test ID".into());
                }
                if selection.test.replace(value).is_some() {
                    return Err("--test may be specified only once".into());
                }
            }
            "--backend" if allow_selection => {
                let value = args.next().ok_or("--backend requires a backend")?;
                if !matches!(
                    value.as_str(),
                    "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst" | "native"
                ) {
                    return Err(format!(
                        "unknown backend `{value}`; expected ptrace, dbt, kvm, sabre, liteinst, or native"
                    ));
                }
                if selection.backend.replace(value).is_some() {
                    return Err("--backend may be specified only once".into());
                }
            }
            "--cell-timeout" if allow_selection => {
                let raw = args.next().ok_or("--cell-timeout requires seconds")?;
                let value = raw.parse::<i64>().map_err(|_| {
                    format!("invalid --cell-timeout `{raw}`; expected positive seconds")
                })?;
                if value <= 0 {
                    return Err("--cell-timeout must be positive".into());
                }
                if selection.cell_timeout_seconds.replace(value).is_some() {
                    return Err("--cell-timeout may be specified only once".into());
                }
            }
            "--sample" if allow_selection => {
                let raw = args.next().ok_or("--sample requires a count")?;
                let value = raw.parse::<usize>().map_err(|_| {
                    format!("invalid --sample `{raw}`; expected a positive integer")
                })?;
                if value == 0 {
                    return Err("--sample must be positive".into());
                }
                if selection.sample.replace(value).is_some() {
                    return Err("--sample may be specified only once".into());
                }
            }
            "--seed" if allow_selection => {
                let raw = args.next().ok_or("--seed requires an unsigned integer")?;
                let value = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --seed `{raw}`; expected an unsigned integer"))?;
                if selection.seed.replace(value).is_some() {
                    return Err("--seed may be specified only once".into());
                }
            }
            "--cells-file" if allow_selection => {
                let raw = args.next().ok_or("--cells-file requires a path")?;
                if raw.is_empty() {
                    return Err("--cells-file requires a nonempty path".into());
                }
                let path = absolute_from(root, PathBuf::from(raw));
                if selection.cells_file.replace(path).is_some() {
                    return Err("--cells-file may be specified only once".into());
                }
            }
            "--run-timeout" if allow_selection => {
                let raw = args.next().ok_or("--run-timeout requires seconds")?;
                let value = raw.parse::<i64>().map_err(|_| {
                    format!("invalid --run-timeout `{raw}`; expected positive seconds")
                })?;
                if value <= 0 {
                    return Err("--run-timeout must be positive".into());
                }
                if selection.run_timeout_seconds.replace(value).is_some() {
                    return Err("--run-timeout may be specified only once".into());
                }
            }
            "--repetitions" if allow_selection => {
                let raw = args.next().ok_or("--repetitions requires a count")?;
                let value = raw.parse::<usize>().map_err(|_| {
                    format!("invalid --repetitions `{raw}`; expected a positive integer")
                })?;
                if value == 0 {
                    return Err("--repetitions must be positive".into());
                }
                if selection.repetitions.replace(value).is_some() {
                    return Err("--repetitions may be specified only once".into());
                }
            }
            "--run-id-prefix" if allow_selection => {
                let value = args.next().ok_or("--run-id-prefix requires a value")?;
                if selection.run_id_prefix.replace(value).is_some() {
                    return Err("--run-id-prefix may be specified only once".into());
                }
            }
            "--green" if allow_selection => {
                if selection.green {
                    return Err("--green may be specified only once".into());
                }
                selection.green = true;
            }
            "--probe-disabled" if allow_selection => {
                if selection.probe_disabled {
                    return Err("--probe-disabled may be specified only once".into());
                }
                selection.probe_disabled = true;
            }
            "--jobs" if allow_selection => {
                let raw = args.next().ok_or("--jobs requires a count")?;
                let value = raw
                    .parse::<i64>()
                    .map_err(|_| format!("invalid --jobs `{raw}`; expected a positive integer"))?;
                if value <= 0 {
                    return Err("--jobs must be positive".into());
                }
                if selection.jobs.replace(value).is_some() {
                    return Err("--jobs may be specified only once".into());
                }
            }
            "--manifest-guest-cap" if allow_selection => {
                let raw = args
                    .next()
                    .ok_or("--manifest-guest-cap requires a count")?;
                let value = raw.parse::<i64>().map_err(|_| {
                    format!(
                        "invalid --manifest-guest-cap `{raw}`; expected a positive integer"
                    )
                })?;
                if value <= 0 {
                    return Err("--manifest-guest-cap must be positive".into());
                }
                if selection.manifest_guest_cap.replace(value).is_some() {
                    return Err("--manifest-guest-cap may be specified only once".into());
                }
            }
            "--kvm-guest-cap" if allow_selection => {
                let raw = args.next().ok_or("--kvm-guest-cap requires a count")?;
                let value = raw.parse::<i64>().map_err(|_| {
                    format!("invalid --kvm-guest-cap `{raw}`; expected a positive integer")
                })?;
                if value <= 0 {
                    return Err("--kvm-guest-cap must be positive".into());
                }
                if selection.kvm_guest_cap.replace(value).is_some() {
                    return Err("--kvm-guest-cap may be specified only once".into());
                }
            }
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    validate_selection_shape(&selection)?;
    // A REPEATED BATCH MAY CARRY IT TOO. The per-cell cap is the bound that
    // actually stops a hung repetition -- the whole-run bound is a wall-clock
    // backstop whose firing means this one did not do its job -- so refusing it
    // on the one selection that reaches the whole population left that
    // population runnable only WITHOUT its inner bound. The original
    // restriction was about not silently capping a set the caller did not
    // choose; a caller who asked for repetitions has chosen one.
    if selection.cell_timeout_seconds.is_some()
        && !(selection.is_exact() || selection.sample.is_some() || selection.repeats_batch())
    {
        return Err("--cell-timeout requires an exact cell, --sample, or a repeated batch".into());
    }
    if selection.sample.is_some() && selection.is_exact() {
        return Err(
            "--sample and an exact --test/--mode/--backend cell are mutually exclusive".into(),
        );
    }
    validate_repetition_selection(&selection)?;
    if selection.seed.is_some() && selection.sample.is_none() {
        return Err("--seed requires --sample".into());
    }
    if selection.sample.is_some() && selection.seed.is_none() {
        selection.seed = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
                .as_nanos() as u64,
        );
    }
    let results = match (results, default_results) {
        (Some(path), _) => absolute_from(root, path),
        (None, true) => default_result_root(root)?,
        (None, false) => return Err("command requires --results DIR".into()),
    };
    let output = output.map(|path| absolute_from(root, path));
    Ok((results, output, selection))
}

fn absolute_from(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn exact_manifest_command_description(dag: &DagConfig, metadata: &RunMetadata) -> Result<String, String> {
    let [cell] = metadata.cells.as_slice() else {
        return Err("exact command display requires precisely one selected cell identity".into());
    };
    if !metadata.is_exact() || metadata.repetitions == Some(0) {
        return Err("exact command display requires a valid exact-cell selection".into());
    }
    let repetition = metadata.repetitions.map(|_| 1);
    let tag = format!("cell.{}", cell_run_slug(cell, repetition));
    let matches: Vec<_> = dag.steps.iter().filter(|step| step.tag() == tag).collect();
    let [step] = matches.as_slice() else {
        return Err(format!("exact command display found {} nodes for {tag}; expected exactly one", matches.len()));
    };
    let policy = metadata.timeout_policy.ok_or("generated command display omitted its timeout policy")?;
    policy.multipliers()?;
    let mut text = format!("Cell: {}/{}/{}\nNode: {tag}", cell.test, cell.mode, cell.backend);
    if let Some(total) = metadata.repetitions {
        text.push_str(&format!(" (repetition 1 of {total})"));
    }
    text.push_str(&format!("\nWall bound: {}s\nCPU bound: {}s\nNode environment:\n",
        step.timeout,
        effective_cpu_timeout(step, dag.default_step_cpu_timeout, dag.cpu_timeout_multiplier)));
    for (name, value) in &step.env {
        text.push_str(&format!("  {name}={}\n", shell_quote(value)));
    }
    text.push_str(&format!(
        "Inherited timeout multipliers recorded for this run:\n  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER={}\n  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER={}\nCommand:\n{}\n",
        policy.cpu_multiplier, policy.wall_multiplier, step.cmd));
    Ok(text)
}

fn print_exact_manifest_command(dag: &DagConfig, metadata: &RunMetadata) -> Result<(), String> {
    print!("{}", exact_manifest_command_description(dag, metadata)?);
    Ok(())
}

fn require_empty_result_dir(results: &Path) -> Result<(), String> {
    if !results.exists() {
        return Ok(());
    }
    if !results.is_dir() {
        return Err(format!(
            "pressure result path is not a directory: {}",
            results.display()
        ));
    }
    let mut entries =
        fs::read_dir(results).map_err(|e| format!("cannot inspect {}: {e}", results.display()))?;
    if entries.next().is_some() {
        return Err(format!(
            "run refuses nonempty result directory {}; choose a fresh directory so old rows cannot satisfy this run",
            results.display()
        ));
    }
    Ok(())
}

/// The repetition ordinal, read from the retained directory name.
///
/// `plan` names a repeated cell `{base}-repetition-{n:04}`. A campaign with no
/// `--repetitions` produces one run of the cell, which is ordinal 0. Returning 0
/// rather than failing is deliberate: a single-run campaign is a series of
/// length one, not an error.
fn series_run_index(dir_name: &str) -> u64 {
    dir_name
        .rsplit_once("-repetition-")
        .and_then(|(_, n)| n.parse::<u64>().ok())
        .unwrap_or(0)
}

/// Publish a retained campaign's per-cell results to the parent's series spool.
///
/// This is the call site the store was missing. Everything it needs already
/// existed: the schema, the linter, the reader, the invariants and the published
/// path. What did not exist was anything that WROTE, so the store stayed empty
/// while four plan steps closed around it.
///
/// It sends the typed cell results to `series.py append-cells`, so outcome,
/// coordinates, source depth, ancestry and compression have one implementation.
/// The writer rejects untyped comparison cells while retaining other valid
/// observations from the campaign. Other malformed input still refuses the
/// batch whole.
fn collect_series_result_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(path)
        .map_err(|e| format!("cannot read pressure result directory {}: {e}", path.display()))?;
    for entry in entries {
        let entry = entry
            .map_err(|e| format!("cannot read pressure result entry under {}: {e}", path.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot classify {}: {e}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_series_result_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == "results.jsonl" {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn collect_series_rows(results: &Path, current_timeouts: bool) -> Result<Vec<(String, CellResult)>, String> {
    let mut result_files = Vec::new();
    collect_series_result_files(results, &mut result_files)?;
    result_files.sort();
    let mut collected: Vec<(String, CellResult)> = Vec::new();
    for result_file in result_files {
        let dir_name = result_file
            .parent()
            .and_then(Path::file_name)
            .ok_or_else(|| format!("{} has no result-directory name", result_file.display()))?
            .to_string_lossy()
            .into_owned();
        let rows = if current_timeouts {
            read_current_result_rows(&result_file)?
        } else {
            read_result_rows(&result_file)?
        };
        for row in rows {
            let repetition = series_run_index(&dir_name);
            if row.run_index != Some(repetition) {
                return Err(format!(
                    "{} records run_index {:?}, but its pressure result directory identifies run {}",
                    result_file.display(),
                    row.run_index,
                    repetition,
                ));
            }
            // Runtime belongs to the typed verification report written by the
            // framework. Retained rows written before that field existed remain
            // readable with `runtime: None`; stderr and retained log prose do not
            // acquire measurement authority after the fact.
            let key = format!(
                "{}/{}/{:020}/{:020}",
                dir_name, row.test, repetition, row.attempt,
            );
            collected.push((key, row));
        }
    }
    collected.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.attempt.cmp(&b.1.attempt))
    });
    Ok(collected)
}

fn emit_series(results: &Path, checkout: &Path, fresh: bool) -> Result<(), String> {
    let parent = std::env::var("DEV_HERMIT_PARENT")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            // Say it, do not skip silently. A campaign that emitted nothing
            // because the parent was not configured must not be indistinguishable
            // from one that had nothing to emit.
            "DEV_HERMIT_PARENT is not set, so there is no series store to write to. \
             Set it to the dev-hermit checkout root and re-run."
                .to_string()
        })?;

    let metadata_path = results.join("run.json");
    let metadata: RunMetadata = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .map_err(|e| format!("cannot read {}: {e}", metadata_path.display()))?,
    )
    .map_err(|e| format!("invalid {}: {e}", metadata_path.display()))?;

    // Deliberately NOT the checkout-HEAD guard `summarize` applies. That guard is
    // right for reading a campaign you are standing in; emitting a RETAINED
    // campaign from a checkout that has since moved is the normal case, and the
    // tree being attributed is recorded in the campaign, not read from git.
    let collected = collect_series_rows(results, current_result_policy(&metadata, fresh)?)?;
    if collected.is_empty() {
        return Err(format!(
            "no per-cell results under {}; nothing to emit",
            results.display()
        ));
    }
    let run_id = if metadata.run_id.is_empty() {
        results
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && *name != "." && *name != "..")
            .ok_or_else(|| format!("{} has no usable run id", results.display()))?
            .to_string()
    } else {
        metadata.run_id.clone()
    };
    let mut payload = String::new();
    for (_, row) in &collected {
        payload.push_str(
            &serde_json::to_string(row).map_err(|e| format!("cannot encode a series row: {e}"))?,
        );
        payload.push('\n');
    }

    let script = Path::new(&parent).join("ci-hub/series/series.py");
    if !script.is_file() {
        return Err(format!(
            "{} does not exist; DEV_HERMIT_PARENT does not look like a dev-hermit checkout",
            script.display()
        ));
    }
    let mut child = Command::new("python3")
        .arg(&script)
        .arg("append-cells")
        .arg("--parent")
        .arg(&parent)
        .arg("--checkout")
        .arg(checkout)
        .arg("--producer")
        .arg("pressure-test")
        .arg("--run-id")
        .arg(&run_id)
        .arg("--tree")
        .arg(&metadata.hermit_sha)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", script.display()))?;
    child
        .stdin
        .take()
        .ok_or("series append stdin unavailable")?
        .write_all(payload.as_bytes())
        .map_err(|e| format!("cannot send rows to the series writer: {e}"))?;
    let status = child
        .wait()
        .map_err(|e| format!("series append did not terminate readably: {e}"))?;
    if !status.success() {
        // A nonzero status keeps every rejected cell visible to the caller.
        // append-cells can still have retained independent valid rows, so do
        // not claim that the whole batch was rolled back.
        return Err(format!(
            "the series writer rejected one or more cell results (exit {:?}); valid completed rows, if any, were retained",
            status.code()
        ));
    }
    println!(
        "emitted {} cell result(s) from run {run_id} to {parent}",
        collected.len()
    );
    Ok(())
}

fn default_result_root(root: &Path) -> Result<PathBuf, String> {
    let sha = git_output(root, &["rev-parse", "--short=12", "HEAD"])?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
        .as_secs();
    Ok(root
        .join("ignored/compat-envelope")
        .join(format!("pressure-{sha}-{now}")))
}

fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err("not inside a Git checkout".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(TRACKED_CELLS).is_file() {
        return Err(format!("{} is not the Hermit checkout", root.display()));
    }
    Ok(root)
}

fn worktree_dirty(root: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect worktree: {e}"))?;
    if !output.status.success() {
        return Err("git status failed".into());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry != b"?? .pressure-test-generated-checkout"))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command_ok(command: &mut Command, purpose: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("cannot {purpose}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cannot {purpose}: {status}"))
    }
}

struct CheckedScorecard<'a> {
    root: &'a Path,
    enforce_host_capabilities: bool,
    memory_budget_override: Option<i64>,
}

fn check_scorecard(root: &Path) -> Result<CheckedScorecard<'_>, String> {
    let status = Command::new(root.join("ci/compat-envelope/scorecard.rs"))
        .arg("check")
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot run scorecard check: {e}"))?;
    if status.success() {
        Ok(CheckedScorecard {
            root,
            enforce_host_capabilities: true,
            memory_budget_override: None,
        })
    } else {
        Err("tracked scorecard is stale; update it before generating a pressure run".into())
    }
}

fn pressure_cells(root: &Path, selection: &CellSelection) -> Result<PressureCells, String> {
    validate_selection_shape(selection)?;
    validate_repetition_selection(selection)?;
    if selection.sample == Some(0) {
        return Err("--sample must be positive".into());
    }
    let budgets = load_budgets(root)?;
    let tracked = load_tracked_cells(root)?;
    let (requested_cells, cells_file_sha256) = if let Some(path) = &selection.cells_file {
        let (cells, digest) = load_cells_file(path)?;
        (Some(cells), Some(digest))
    } else if let Some(cells) = &selection.retained_cells_file_cells {
        if cells.is_empty() {
            return Err("retained --cells-file selection is empty".into());
        }
        let unique: BTreeSet<_> = cells.iter().cloned().collect();
        if unique.len() != cells.len() {
            return Err("retained --cells-file selection contains a duplicate identity".into());
        }
        (Some(cells.clone()), None)
    } else {
        (None, None)
    };
    let requested_ids = requested_cells
        .as_ref()
        .map(|cells| cells.iter().cloned().collect::<BTreeSet<_>>());
    let mut matched_requested = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut selected_cells = Vec::new();
    let mut unavailable = Vec::new();
    let mut enabled_by_test = BTreeMap::new();
    for cell in tracked.cells {
        if !seen.insert(cell.id.clone()) {
            return Err("tracked cells contain a duplicate identity".into());
        }
        if cell.enabled {
            enabled_by_test
                .entry(cell.id.test.clone())
                .or_insert_with(|| cell.id.clone());
        }
        let selected = if let Some(requested) = &requested_ids {
            let selected = requested.contains(&cell.id);
            if selected {
                matched_requested.insert(cell.id.clone());
            }
            selected
        } else {
            selection
                .mode
                .as_deref()
                .is_none_or(|value| cell.id.mode == value)
                && selection
                    .test
                    .as_deref()
                    .is_none_or(|value| cell.id.test == value)
                && selection
                    .backend
                    .as_deref()
                    .is_none_or(|value| cell.id.backend == value)
                && !(selection.sample.is_some()
                    && selection.mode.is_none()
                    && !matches!(cell.id.mode.as_str(), "verify" | "replay" | "chaos"))
        };
        match cell.status.as_str() {
            "red"
                if selected
                    && !selection.selects_green_population()
                    && !selection.probe_disabled =>
            {
                if requested_ids.is_some() && !cell.enabled {
                    return Err(format!(
                        "--cells-file identity {} is tracked but disabled",
                        display_id(&cell.id)
                    ));
                }
                let budget = budgets
                    .get(&(
                        cell.id.test.clone(),
                        cell.id.mode.clone(),
                        cell.id.backend.clone(),
                    ))
                    .ok_or_else(|| {
                        format!(
                            "no manifest execution budget for {}/{}/{}",
                            cell.id.test, cell.id.mode, cell.id.backend
                        )
                    })?;
                if budget.attempts.is_some() {
                    selected_cells.push(cell);
                } else if requested_ids.is_some() {
                    return Err(format!(
                        "--cells-file identity {} is tracked but its manifest declares no executable attempts",
                        display_id(&cell.id)
                    ));
                } else if selection.is_exact() {
                    return Err(format!(
                        "{}/{}/{} is red but unavailable: its manifest declares no chaos seeds, so there is no guest command to run",
                        cell.id.test, cell.id.mode, cell.id.backend
                    ));
                } else {
                    unavailable.push(cell);
                }
            }
            "red" => {}
            "green" if selected && requested_ids.is_some() => {
                return Err(format!(
                    "--cells-file identity {} is green, not in the red pressure population",
                    display_id(&cell.id)
                ));
            }
            "green" if selected && selection.selects_green_population() && cell.enabled => {
                let budget = budgets
                    .get(&(
                        cell.id.test.clone(),
                        cell.id.mode.clone(),
                        cell.id.backend.clone(),
                    ))
                    .ok_or_else(|| {
                        format!(
                            "no manifest execution budget for {}/{}/{}",
                            cell.id.test, cell.id.mode, cell.id.backend
                        )
                    })?;
                if budget.attempts.is_none() {
                    return Err(format!(
                        "{}/{}/{} is green but its manifest has no executable attempt recipe",
                        cell.id.test, cell.id.mode, cell.id.backend
                    ));
                }
                selected_cells.push(cell);
            }
            "green" => {}
            "not-applicable" if selected && requested_ids.is_some() => {
                return Err(format!(
                    "--cells-file identity {} is unsupported: {}",
                    display_id(&cell.id),
                    cell.not_applicable_reason.as_deref().unwrap_or(
                        "its backend is not enabled for this mode, so it has no guest command"
                    )
                ));
            }
            "not-applicable" if selected && selection.probe_disabled => {
                let budget = budgets
                    .get(&(
                        cell.id.test.clone(),
                        cell.id.mode.clone(),
                        cell.id.backend.clone(),
                    ))
                    .ok_or_else(|| {
                        format!(
                            "no manifest execution budget for {}/{}/{}",
                            cell.id.test, cell.id.mode, cell.id.backend
                        )
                    })?;
                if budget.attempts.is_some() {
                    selected_cells.push(cell);
                } else if selection.is_exact() {
                    return Err(format!(
                        "{}/{}/{} is disabled and unavailable: its manifest declares no executable attempts",
                        cell.id.test, cell.id.mode, cell.id.backend
                    ));
                } else {
                    unavailable.push(cell);
                }
            }
            // Without explicit probing, a disabled cell stays out of the
            // executable population and an exact request explains why.
            "not-applicable" if selected && selection.is_exact() => {
                return Err(format!(
                    "{}/{}/{} is NOT APPLICABLE, not red: {}",
                    cell.id.test,
                    cell.id.mode,
                    cell.id.backend,
                    cell.not_applicable_reason.as_deref().unwrap_or(
                        "its backend is not enabled for this mode, so it has no guest command"
                    )
                ));
            }
            "not-applicable" => {}
            other => return Err(format!("unknown cell status `{other}`")),
        }
    }
    if let Some(requested) = &requested_ids {
        if let Some(missing) = requested.difference(&matched_requested).next() {
            return Err(format!(
                "--cells-file identity {} is not present in the tracked scorecard",
                display_id(missing)
            ));
        }
    }
    selected_cells.sort_by(|left, right| left.id.cmp(&right.id));
    if selected_cells.is_empty() {
        let population = population_label(selection.green, selection.probe_disabled);
        if !unavailable.is_empty() {
            return Err(format!(
                "the selected {population} population has no executable commands; {} cell(s) are unavailable because their manifests declare no executable attempts",
                unavailable.len()
            ));
        }
        return Err(
            if let (Some(test), Some(mode), Some(backend)) = (
                selection.test.as_deref(),
                selection.mode.as_deref(),
                selection.backend.as_deref(),
            ) {
                if selection.selects_green_population() {
                    format!(
                        "{test}/{mode}/{backend} is not an enabled green tracked cell; use the scorecard or manifest CLI to inspect it"
                    )
                } else if selection.probe_disabled {
                    format!(
                        "{test}/{mode}/{backend} is not a disabled tracked cell; use the scorecard or manifest CLI to inspect it"
                    )
                } else {
                    format!(
                        "{test}/{mode}/{backend} is not a currently red tracked cell; use the scorecard or manifest CLI to inspect it"
                    )
                }
            } else if let Some(mode) = selection.mode.as_deref() {
                if selection.selects_green_population() {
                    format!("tracked scorecard has no enabled green cells for mode `{mode}`")
                } else if selection.probe_disabled {
                    format!("tracked scorecard has no disabled cells for mode `{mode}`")
                } else {
                    format!("tracked scorecard has no red cells for mode `{mode}`")
                }
            } else if selection.selects_green_population() {
                "tracked scorecard has no enabled green cells".into()
            } else if selection.probe_disabled {
                "tracked scorecard has no disabled cells".into()
            } else {
                "tracked scorecard has no red cells".into()
            },
        );
    }
    let eligible_cells = selected_cells.len();
    if let Some(count) = selection.sample {
        if count > selected_cells.len() {
            return Err(if selection.selects_green_population() {
                format!(
                    "--sample {count} exceeds the {} enabled green cells in the selected population",
                    selected_cells.len()
                )
            } else {
                let population = population_label(selection.green, selection.probe_disabled);
                format!(
                    "--sample {count} exceeds the {} {population} cells with executable commands in the selected population; {} selected {population} cell(s) are unavailable because their manifests declare no executable attempts",
                    selected_cells.len(),
                    unavailable.len()
                )
            });
        }
        let seed = selection
            .seed
            .ok_or("--sample requires a retained seed before selecting cells")?;
        selected_cells.sort_by(|left, right| {
            sample_score(&left.id, seed)
                .cmp(&sample_score(&right.id, seed))
                .then_with(|| left.id.cmp(&right.id))
        });
        selected_cells.truncate(count);
        selected_cells.sort_by(|left, right| left.id.cmp(&right.id));
    }
    let mut preparation_by_test = BTreeMap::new();
    for cell in &selected_cells {
        let prepared_with = enabled_by_test.get(&cell.id.test).ok_or_else(|| {
            format!(
                "{} has no manifest-enabled mode available to build its fixture",
                cell.id.test
            )
        })?;
        preparation_by_test
            .entry(cell.id.test.clone())
            .or_insert_with(|| prepared_with.clone());
    }
    Ok(PressureCells {
        selected: selected_cells,
        unavailable,
        eligible_cells,
        preparation_by_test,
        cells_file_sha256,
    })
}

/// Stable seeded ordering for a retained random sample. The selected identities
/// are also written to run.json, so replay does not depend on this arithmetic
/// remaining unchanged across future tool versions.
fn sample_score(cell: &CellId, seed: u64) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64 ^ seed;
    for byte in display_id(cell).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // SplitMix64 finalizer: deterministic, inexpensive, and sufficiently
    // well-distributed for choosing a diagnostic sample without a new runtime
    // dependency.
    let mut value = hash.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn load_budgets(root: &Path) -> Result<BTreeMap<(String, String, String), CellBudget>, String> {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "hermit-manifest-plan",
            "--",
            "--format",
            "matrix-json",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run hermit-manifest-plan: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "hermit-manifest-plan failed while loading execution budgets:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    decode_budgets(&output.stdout)
}

fn decode_budgets(
    matrix_json: &[u8],
) -> Result<BTreeMap<(String, String, String), CellBudget>, String> {
    let rows: Vec<ManifestBudgetRow> = serde_json::from_slice(matrix_json)
        .map_err(|e| format!("manifest-plan emitted invalid matrix JSON: {e}"))?;
    if rows.is_empty() {
        return Err("manifest-plan emitted an empty matrix".into());
    }
    let mut out: BTreeMap<(String, String, String), CellBudget> = BTreeMap::new();
    for row in rows {
        if !(1..=1800).contains(&row.timeout_seconds) {
            return Err(format!(
                "manifest-plan emitted timeout {} outside 1..=1800 for {}/{}/{}",
                row.timeout_seconds, row.test, row.mode, row.backend
            ));
        }
        resolve_test_timeouts(
            u64::try_from(row.cpu_timeout_seconds).map_err(|_| "negative manifest CPU timeout")?,
            row.timeout_seconds as u64,
            TimeoutMultipliers::default(),
        )?;
        let attempts = if row.attempts.is_null() {
            None
        } else {
            Some(row.attempts.as_i64().ok_or_else(|| {
                format!(
                    "manifest-plan emitted a non-integer attempt count for {}/{}/{}",
                    row.test, row.mode, row.backend
                )
            })?)
        };
        if attempts.is_none() && row.mode != "chaos" {
            return Err(format!(
                "manifest-plan emitted no attempt count for non-chaos mode {}/{}/{}",
                row.test, row.mode, row.backend
            ));
        }
        if attempts.is_some_and(|attempts| attempts <= 0) {
            return Err(format!(
                "manifest-plan emitted a nonpositive attempt count for {}/{}/{}",
                row.test, row.mode, row.backend
            ));
        }
        let key = (row.test, row.mode, row.backend);
        let budget = CellBudget {
            cpu_timeout_seconds: row.cpu_timeout_seconds,
            timeout_seconds: row.timeout_seconds,
            attempts,
        };
        if let Some(existing) = out.get(&key) {
            if existing != &budget {
                return Err(format!(
                    "manifest-plan emitted conflicting execution budgets for {}/{}/{}",
                    key.0, key.1, key.2
                ));
            }
        } else {
            out.insert(key, budget);
        }
    }
    Ok(out)
}

/// Each framework attempt gets a preparation deadline and a fresh execution
/// deadline. Internal runs/seeds share the execution budget; they do not multiply
/// it. Allow the existing 10s TERM/KILL grace per attempt, then 30s for reporting.
/// `budget` already contains the separately resolved CPU and wall limits.
fn outer_timeout(budget: &CellBudget) -> Result<i64, String> {
    budget.attempts.ok_or(
        "cannot derive a wall cap for a cell whose manifest has no executable attempt recipe",
    )?;
    budget.timeout_seconds.checked_mul(2)
        .and_then(|seconds| seconds.checked_add(10))
        .and_then(|seconds| seconds.checked_mul(MAX_ATTEMPTS_PER_CELL as i64))
        .and_then(|seconds| seconds.checked_add(30))
        .ok_or_else(|| "cell retry lifecycle exceeds the supported integer range".into())
}

fn pressure_timeout(budget: &CellBudget, selected_cap: Option<i64>) -> Result<i64, String> {
    let required = outer_timeout(budget)?;
    if let Some(cap) = selected_cap {
        if cap < required {
            return Err(format!(
                "--cell-timeout {cap}s is shorter than the required {required}s enclosing lifecycle; refusing before launch without changing the requested cap or any inner timeout"
            ));
        }
    }
    required.checked_mul(2).ok_or("cell outer CPU bound exceeds the supported integer range")?;
    Ok(required)
}

/// Exact old rule, solely for reading plans written before timeout_policy.
fn legacy_pressure_timeout(budget: &CellBudget, selected_cap: Option<i64>) -> Result<i64, String> {
    budget.attempts.ok_or(
        "cannot derive a wall cap for a cell whose manifest has no executable attempt recipe",
    )?;
    Ok((budget.timeout_seconds + 10 + 30).min(
        selected_cap.unwrap_or(LEGACY_PRESSURE_CELL_TIMEOUT_SECONDS)
            .min(LEGACY_PRESSURE_CELL_TIMEOUT_SECONDS),
    ))
}

/// Shared preparation runs once, outside the framework retry loop. Preserve its
/// own wall deadline, TERM/KILL grace, and reporting allowance independently.
fn preparation_timeout(budget: &CellBudget) -> Result<i64, String> {
    budget.timeout_seconds.checked_add(10 + 30)
        .ok_or_else(|| "preparation timeout exceeds the supported integer range".into())
}

fn preparation_node_timeout(budget: &CellBudget) -> Result<i64, String> {
    preparation_timeout(budget)?.checked_add(20)
        .ok_or_else(|| "preparation node timeout exceeds the supported integer range".into())
}

fn require_generated_node_count(
    cells: usize, repetitions: usize, preparations: usize, builds: usize,
) -> Result<usize, String> {
    let count = cells.checked_mul(repetitions)
        .and_then(|count| count.checked_add(preparations))
        .and_then(|count| count.checked_add(builds))
        .and_then(|count| count.checked_add(1))
        .ok_or("--repetitions produces an unrepresentable generated-node count")?;
    if count > MAX_PRESSURE_GENERATED_NODES {
        return Err(format!(
            "--repetitions would generate {count} nodes, above the {MAX_PRESSURE_GENERATED_NODES}-node safety bound"
        ));
    }
    Ok(count)
}

fn cell_memory_bytes(cell: &CellId) -> i64 {
    if cell.lane == "privileged" || cell.backend == "kvm" {
        PRIVILEGED_CELL_MEMORY_BYTES
    } else {
        PORTABLE_CELL_MEMORY_BYTES
    }
}

fn checked_memory_sum(mut caps: impl Iterator<Item = i64>) -> Result<i64, String> {
    caps.try_fold(0_i64, |sum, cap| {
        sum.checked_add(cap)
            .ok_or_else(|| "declared pressure-test memory caps overflow".into())
    })
}

/// Establish the phase and resource assumptions used by the memory upper bound.
/// The initial sum is conservative only when every later producer/consumer waits
/// for all initial nodes. A future graph change must satisfy this check explicitly.
fn require_accounted_memory_phases(dag: &DagConfig) -> Result<(), String> {
    let mut steps = BTreeMap::new();
    let mut early = BTreeSet::new();
    let mut kvm_caps = Vec::new();
    let mut other_caps = Vec::new();
    for step in &dag.steps {
        let tag = step.tag();
        if steps.insert(tag.clone(), step).is_some() {
            return Err(format!("memory admission has duplicate step {tag}"));
        }
        let cap = step.hint.hard_mem_max_bytes.filter(|cap| *cap > 0)
            .ok_or_else(|| format!("{tag} has no positive hard memory cap"))?;
        match step.group.as_str() {
            "pre" | "gate" | "setup" | "build" => {
                if tag != "build.liteinst_runtime_release" {
                    early.insert(tag);
                }
            }
            "prepare" => {
                if dag.resource_caps.get("cargo_writer") != Some(&1)
                    || step.hint.resources.get("cargo_writer") != Some(&1)
                {
                    return Err(format!("{tag} lacks the single cargo_writer preparation bound"));
                }
            }
            "cell" => {
                if step.hint.resources.get("manifest_guest") != Some(&1) {
                    return Err(format!("{tag} lacks unit manifest_guest demand"));
                }
                match step.hint.resources.get("kvm_guest").copied() {
                    Some(1) => kvm_caps.push(cap),
                    None => other_caps.push(cap),
                    Some(_) => return Err(format!("{tag} has non-unit KVM guest demand")),
                }
            }
            "pressure" if tag == "pressure.summarize" => {}
            _ => return Err(format!("memory admission has unaccounted step {tag}")),
        }
    }
    // The cell maximum takes KVM slots first. This is conservative only when
    // none of the remaining cells can cost more than a KVM cell (ties are valid).
    if let (Some(kvm_min), Some(other_max)) = (kvm_caps.iter().min(), other_caps.iter().max()) {
        if kvm_min < other_max {
            return Err("KVM-first memory admission requires every KVM cap to cover every non-KVM cap".into());
        }
    }
    for (tag, step) in &steps {
        let mut ancestors = BTreeSet::new();
        let mut pending = step.deps.clone();
        while let Some(dependency) = pending.pop() {
            if &dependency == tag {
                return Err(format!("memory admission found a dependency cycle at {tag}"));
            }
            if ancestors.insert(dependency.clone()) {
                let producer = steps.get(&dependency)
                    .ok_or_else(|| format!("memory admission found absent dependency {dependency} of {tag}"))?;
                pending.extend(producer.deps.iter().cloned());
            }
        }
        if matches!(step.group.as_str(), "prepare" | "cell")
            || tag == "build.liteinst_runtime_release"
        {
            if let Some(missing) = early.difference(&ancestors).next() {
                return Err(format!("memory phase for {tag} does not wait for initial node {missing}"));
            }
        }
        if tag == "pressure.summarize" {
            if let Some(missing) = steps.keys().find(|other| *other != tag && !ancestors.contains(*other)) {
                return Err(format!("memory phase for {tag} can overlap unfinished node {missing}"));
            }
        }
    }
    Ok(())
}

/// Conservative peak for every phase of the generated graph.
///
/// All early preflight, gate, setup and build caps are summed. During execution,
/// `cargo_writer=1` permits one preparation, and the independent late LiteInst
/// build may also overlap the largest runnable cell caps. The explicit control-plane reserve is outside every
/// child cgroup and is therefore added after choosing the largest phase.
fn declared_memory_at_manifest_guest_cap(
    dag: &DagConfig,
    jobs: i64,
    manifest_guest_cap: i64,
    kvm_guest_cap: i64,
) -> Result<i64, String> {
    if jobs <= 0 || manifest_guest_cap <= 0 || kvm_guest_cap <= 0 {
        return Err("pressure-test scheduler, manifest guest, and KVM caps must be positive".into());
    }
    require_accounted_memory_phases(dag)?;
    let cap_of = |step: &Step| {
        step.hint
            .hard_mem_max_bytes
            .filter(|cap| *cap > 0)
            .ok_or_else(|| format!("{} has no positive hard memory cap", step.tag()))
    };
    let early = checked_memory_sum(
        dag.steps
            .iter()
            .filter(|step| {
                matches!(step.group.as_str(), "pre" | "gate" | "setup" | "build")
                    && step.tag() != "build.liteinst_runtime_release"
            })
            .map(cap_of)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter(),
    )?;
    let late_build = dag
        .steps
        .iter()
        .filter(|step| step.tag() == "build.liteinst_runtime_release")
        .map(cap_of)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let preparation = dag
        .steps
        .iter()
        .filter(|step| step.group == "prepare")
        .map(cap_of)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let summary = dag
        .steps
        .iter()
        .filter(|step| step.group == "pressure")
        .map(cap_of)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    let mut kvm_cell_caps = dag
        .steps
        .iter()
        .filter(|step| {
            step.group == "cell" && step.hint.resources.get("kvm_guest") == Some(&1)
        })
        .map(cap_of)
        .collect::<Result<Vec<_>, _>>()?;
    let mut portable_cell_caps = dag
        .steps
        .iter()
        .filter(|step| {
            step.group == "cell" && step.hint.resources.get("kvm_guest").copied().unwrap_or(0) == 0
        })
        .map(cap_of)
        .collect::<Result<Vec<_>, _>>()?;
    kvm_cell_caps.sort_unstable_by(|left, right| right.cmp(left));
    portable_cell_caps.sort_unstable_by(|left, right| right.cmp(left));
    let cell_sum = |width: i64| {
        let width = usize::try_from(width).unwrap_or(usize::MAX);
        let kvm_width = usize::try_from(kvm_guest_cap)
            .unwrap_or(usize::MAX)
            .min(width)
            .min(kvm_cell_caps.len());
        let portable_width = width
            .saturating_sub(kvm_width)
            .min(portable_cell_caps.len());
        checked_memory_sum(
            kvm_cell_caps[..kvm_width]
                .iter()
                .chain(portable_cell_caps[..portable_width].iter())
                .copied(),
        )
    };
    let cell_width = jobs.min(manifest_guest_cap);
    let cells_only = cell_sum(cell_width)?;
    let support_with_cells = |support: i64, support_slots: i64| -> Result<i64, String> {
        support
            .checked_add(cell_sum(
                cell_width.min(jobs.saturating_sub(support_slots)),
            )?)
            .ok_or_else(|| "declared pressure-test memory caps overflow".into())
    };
    let late_overlap = support_with_cells(late_build, i64::from(late_build > 0))?;
    let preparation_overlap = support_with_cells(preparation, i64::from(preparation > 0))?;
    // LiteInst's late runtime build and preparation for another test are both
    // scheduler-reachable while already-prepared non-LiteInst cells run.
    let combined_support = late_build
        .checked_add(preparation)
        .ok_or("declared pressure-test memory caps overflow")?;
    let combined_overlap = support_with_cells(
        combined_support,
        i64::from(late_build > 0) + i64::from(preparation > 0),
    )?;
    early
        .max(cells_only)
        .max(late_overlap)
        .max(preparation_overlap)
        .max(combined_overlap)
        .max(summary)
        .checked_add(CONTROL_PLANE_HEADROOM_BYTES)
        .ok_or_else(|| "pressure-test control-plane headroom overflows".into())
}

fn max_safe_manifest_guest_effective_width(
    dag: &DagConfig,
    jobs: i64,
    kvm_guest_cap: i64,
    budget: i64,
) -> Result<i64, String> {
    if budget <= 0 {
        return Ok(0);
    }
    let total_cells = i64::try_from(
        dag.steps
            .iter()
            .filter(|step| step.group == "cell")
            .count(),
    )
    .unwrap_or(i64::MAX);
    let mut low = 0_i64;
    let mut high = jobs.min(total_cells);
    while low < high {
        let candidate = low + (high - low + 1) / 2;
        if declared_memory_at_manifest_guest_cap(dag, jobs, candidate.max(1), kvm_guest_cap)?
            <= budget
        {
            low = candidate;
        } else {
            high = candidate - 1;
        }
    }
    Ok(low)
}

fn max_safe_kvm_guest_cap(
    dag: &DagConfig,
    jobs: i64,
    manifest_guest_cap: i64,
    budget: i64,
) -> Result<i64, String> {
    if budget <= 0 {
        return Ok(0);
    }
    let total_kvm = i64::try_from(
        dag.steps
            .iter()
            .filter(|step| {
                step.group == "cell" && step.hint.resources.get("kvm_guest") == Some(&1)
            })
            .count(),
    )
    .unwrap_or(i64::MAX);
    if total_kvm == 0 {
        return Ok(0);
    }
    let mut low = 0_i64;
    let mut high = jobs.min(manifest_guest_cap).min(total_kvm);
    while low < high {
        let candidate = low + (high - low + 1) / 2;
        if declared_memory_at_manifest_guest_cap(dag, jobs, manifest_guest_cap, candidate.max(1))?
            <= budget
        {
            low = candidate;
        } else {
            high = candidate - 1;
        }
    }
    Ok(low)
}

fn validate_manifest_guest_memory(
    dag: &DagConfig,
    selection: &CellSelection,
    observed_budget: Option<i64>,
) -> Result<(Option<i64>, i64, Option<i64>, Option<i64>), String> {
    let required = declared_memory_at_manifest_guest_cap(
        dag,
        selection.scheduler_jobs(),
        selection.manifest_guest_cap(),
        selection.kvm_guest_cap(),
    )?;
    let max_safe_cap = observed_budget
        .map(|budget| {
            max_safe_manifest_guest_effective_width(
                dag,
                selection.scheduler_jobs(),
                selection.kvm_guest_cap(),
                budget,
            )
        })
        .transpose()?;
    let max_safe_kvm_cap = observed_budget
        .map(|budget| {
            max_safe_kvm_guest_cap(
                dag,
                selection.scheduler_jobs(),
                selection.manifest_guest_cap(),
                budget,
            )
        })
        .transpose()?;
    if selection.manifest_guest_cap.is_some() || selection.kvm_guest_cap.is_some() {
        let budget = observed_budget.ok_or(
            "--manifest-guest-cap refuses because the cgroup/machine memory budget is unreadable",
        )?;
        if budget <= 0 || required > budget {
            return Err(format!(
                "--manifest-guest-cap {} is unsafe: the generated DAG's concurrent hard caps plus {} bytes of control-plane headroom require {required} bytes, exceeding the observed cgroup/machine budget of {budget} bytes; highest safe cap for this population at -j {} is {}",
                selection.manifest_guest_cap(),
                CONTROL_PLANE_HEADROOM_BYTES,
                selection.scheduler_jobs(),
                max_safe_cap.unwrap_or(0)
            ));
        }
    }
    Ok((observed_budget, required, max_safe_cap, max_safe_kvm_cap))
}

fn observed_manifest_guest_memory_budget() -> Option<i64> {
    [
        box_mem_budget_bytes(),
        dagrun::cgroup::outer_memory_max_bytes(),
        dagrun::cgroup::expected_outer_memory_max_bytes(),
    ]
    .into_iter()
    .flatten()
    .filter(|bytes| *bytes > 0)
    .min()
}

fn strict_kvm_capability() -> CapabilityVerdict {
    let declared = hermit_manifest_plan::host_capability::probe_host_capability(HostCapability::Kvm);
    match fs::OpenOptions::new().read(true).write(true).open("/dev/kvm") {
        Ok(_) if declared.present => CapabilityVerdict {
            present: true,
            evidence: format!("{}; /dev/kvm is openable read-write", declared.evidence),
        },
        Ok(_) => CapabilityVerdict {
            present: false,
            evidence: format!(
                "canonical KVM capability probe refused despite openable /dev/kvm: {}",
                declared.evidence
            ),
        },
        Err(error) => CapabilityVerdict {
            present: false,
            evidence: format!("{}; cannot open /dev/kvm read-write: {error}", declared.evidence),
        },
    }
}

fn require_selected_kvm_capability(
    cells: &[TrackedCell],
    verdict: &CapabilityVerdict,
) -> Result<(), String> {
    if cells.iter().any(|cell| cell.id.backend == "kvm") && !verdict.present {
        return Err(format!(
            "selected KVM cells are not executable on this host: {}",
            verdict.evidence
        ));
    }
    Ok(())
}

fn validate_guest_caps_against_selected_demand(
    cells: &[TrackedCell],
    selection: &CellSelection,
) -> Result<(), String> {
    let repetitions = i64::try_from(selection.run_count())
        .map_err(|_| "--repetitions is too large for the guest-cap demand calculation")?;
    let total_runs = i64::try_from(cells.len())
        .unwrap_or(i64::MAX)
        .checked_mul(repetitions)
        .ok_or("selected cell count overflows the guest-cap demand calculation")?;
    if let Some(cap) = selection.manifest_guest_cap {
        let effective_demand = selection.scheduler_jobs().min(total_runs);
        if cap > effective_demand {
            return Err(format!(
                "--manifest-guest-cap {cap} exceeds selected effective demand {effective_demand} at -j {}; lower the cap to {effective_demand}",
                selection.scheduler_jobs()
            ));
        }
    }
    if let Some(cap) = selection.kvm_guest_cap {
        let kvm_cells = i64::try_from(
            cells
                .iter()
                .filter(|cell| cell.id.backend == "kvm")
                .count(),
        )
        .unwrap_or(i64::MAX);
        let kvm_runs = kvm_cells
            .checked_mul(repetitions)
            .ok_or("selected KVM cell count overflows the guest-cap demand calculation")?;
        let effective_demand = selection
            .scheduler_jobs()
            .min(selection.manifest_guest_cap())
            .min(kvm_runs);
        if cap > effective_demand {
            let remedy = if effective_demand == 0 {
                "omit --kvm-guest-cap".to_string()
            } else {
                format!("lower the cap to {effective_demand}")
            };
            return Err(format!(
                "--kvm-guest-cap {cap} exceeds selected effective KVM demand {effective_demand}; {remedy}"
            ));
        }
    }
    Ok(())
}

fn require_cell_occupancy_fits(
    cells: &[TrackedCell],
    budgets: &BTreeMap<(String, String, String), CellBudget>,
    selected_cap: Option<i64>,
    run_timeout_seconds: i64,
    repetitions: usize,
    jobs: i64,
    manifest_guest_cap: i64,
    kvm_guest_cap: i64,
) -> Result<(), String> {
    let repetitions = i64::try_from(repetitions).map_err(|_| {
        "--repetitions is too large to represent in the pressure-test occupancy calculation"
            .to_string()
    })?;
    let mut all_seconds = 0_i64;
    let mut kvm_seconds = 0_i64;
    for tracked in cells {
        let budget = budgets
            .get(&(
                tracked.id.test.clone(),
                tracked.id.mode.clone(),
                tracked.id.backend.clone(),
            ))
            .ok_or_else(|| {
                format!(
                    "no manifest budget for {}/{}/{}",
                    tracked.id.test, tracked.id.mode, tracked.id.backend
                )
            })?;
        let seconds = pressure_timeout(budget, selected_cap)?;
        let seconds = seconds.checked_mul(repetitions).ok_or_else(|| {
            "--repetitions makes the declared pressure-test occupancy exceed the supported integer range"
                .to_string()
        })?;
        all_seconds = all_seconds.checked_add(seconds).ok_or_else(|| {
            "the selected cells make the declared pressure-test occupancy exceed the supported integer range"
                .to_string()
        })?;
        if tracked.id.backend == "kvm" {
            kvm_seconds = kvm_seconds.checked_add(seconds).ok_or_else(|| {
                "the selected KVM cells exceed the supported occupancy range".to_string()
            })?;
        }
    }
    // The generated graph permits at most the retained manifest guest cap. If
    // every selected cell consumes its declared cap, this resource limit imposes
    // this minimum wall time even before build and preparation work. Refuse an
    // impossible public bound instead of printing a command which cannot satisfy
    // its own contract.
    let guest_width = jobs.clamp(1, manifest_guest_cap);
    let guest_floor = all_seconds / guest_width + i64::from(all_seconds % guest_width != 0);
    let kvm_width = jobs.min(manifest_guest_cap).clamp(1, kvm_guest_cap);
    let kvm_floor = kvm_seconds / kvm_width + i64::from(kvm_seconds % kvm_width != 0);
    let occupancy_floor = guest_floor.max(kvm_floor);
    if occupancy_floor >= run_timeout_seconds {
        return Err(format!(
            "selected {} cell run(s) have at least {occupancy_floor}s of declared worst-case cell occupancy at -j {jobs}, manifest_guest={manifest_guest_cap}, and kvm_guest={kvm_guest_cap}, which cannot fit the {run_timeout_seconds}s whole-run WALL bound; use --sample, reduce --repetitions, adjust safe guest caps, or deliberately raise --run-timeout",
            i64::try_from(cells.len())
                .unwrap_or(i64::MAX)
                .saturating_mul(repetitions)
        ));
    }
    Ok(())
}

fn build_marker(results: &Path, tag: &str) -> PathBuf {
    results.join("state").join(format!("{}.ok", sanitize(tag)))
}

fn required_build_tags(
    exact_cell: Option<(&str, &str)>,
    includes_liteinst: bool,
) -> BTreeSet<&'static str> {
    // Keep the explicit canonical prerequisite chain, including the scripts
    // required by the copied commands. The plan-time scorecard check does not
    // replace gate.manifest. Exact native cells need only the manifest tool;
    // other exact non-LiteInst cells add the gated runtime build. Batch and
    // LiteInst cells retain the canonical artifact producers.
    if let Some((mode, backend)) = exact_cell {
        let mut required = BTreeSet::from([
            "pre.submodules", "pre.reverie_pin", "build.rust_scripts",
            "setup.manifest_plan",
        ]);
        if mode == "naked" && backend == "native" {
            return required;
        }
        if backend != "liteinst" {
            required.extend(["gate.manifest", "build.runtime_release"]);
            return required;
        }
    }
    REQUIRED_BUILD_TAGS
        .into_iter()
        .filter(|tag| includes_liteinst || *tag != "build.liteinst_runtime_release")
        .collect()
}

fn required_builds_complete(results: &Path, metadata: &RunMetadata) -> bool {
    let exact_cell = (metadata.test.is_some()
        && metadata.mode.is_some()
        && metadata.backend.is_some()
        && metadata.cells.len() == 1)
        .then(|| {
            (
                metadata.mode.as_deref().expect("checked exact mode"),
                metadata.backend.as_deref().expect("checked exact backend"),
            )
        });
    let includes_liteinst = metadata.cells.iter().any(|cell| cell.backend == "liteinst");
    required_build_tags(exact_cell, includes_liteinst)
        .iter()
        .all(|tag| build_marker(results, tag).is_file())
}

fn selected_cell_dependencies(
    exact_cell: bool,
    shared_preparation: bool,
    mode: &str,
    backend: &str,
    preparation_tag: Option<&str>,
) -> Vec<String> {
    if exact_cell {
        let mut deps = vec!["setup.manifest_plan".into()];
        if shared_preparation {
            deps.push(
                preparation_tag
                    .expect("shared exact cell has a preparation tag")
                    .into(),
            );
        }
        if !(mode == "naked" && backend == "native") {
            deps.push(if backend == "liteinst" {
                "build.liteinst_runtime_release".into()
            } else {
                "build.runtime_release".into()
            });
        }
        return deps;
    }
    let mut deps = vec![
        "setup.manifest_plan".into(),
        preparation_tag
            .expect("batch cell has a preparation tag")
            .into(),
        "build.e2e_artifact".into(),
    ];
    if backend == "liteinst" {
        deps.push("build.liteinst_runtime_release".into());
    }
    deps
}

fn retain_required_build_dependencies(
    step: &mut Step,
    required_builds: &BTreeSet<&str>,
) -> Result<(), String> {
    let tag = step.tag();
    // The selected set is explicit. Never import an arbitrary future closure,
    // and never omit a prerequisite of a copied canonical command.
    for dependency in &step.deps {
        if !required_builds.contains(dependency.as_str()) {
            return Err(format!(
                "canonical build node {tag} has unexpected prerequisite {dependency}; refusing to omit a prerequisite whose effect on the consumed build artifacts is unknown"
            ));
        }
    }
    Ok(())
}

fn base_cell_slug(cell: &CellId) -> String {
    sanitize(&format!(
        "{}-{}-{}-{}-{}",
        cell.lane, cell.category, cell.test, cell.mode, cell.backend
    ))
}

fn repetition_numbers(repetitions: Option<usize>) -> impl Iterator<Item = Option<usize>> {
    let count = repetitions.unwrap_or(1);
    (1..=count).map(move |number| repetitions.map(|_| number))
}

fn cell_run_slug(cell: &CellId, repetition: Option<usize>) -> String {
    let base = base_cell_slug(cell);
    repetition.map_or(base.clone(), |number| {
        format!("{base}-repetition-{number:04}")
    })
}

fn cell_evidence_run_id(
    cell: &CellId,
    repetition: Option<usize>,
    run_id_prefix: Option<&str>,
) -> String {
    let slug = cell_run_slug(cell, repetition);
    run_id_prefix.map_or_else(|| slug.clone(), |prefix| format!("{prefix}--{slug}"))
}

fn write_plan(
    root: &Path,
    results: &Path,
    output: &Path,
    selection: &CellSelection,
) -> Result<(RunMetadata, DagConfig), String> {
    let checked_scorecard = check_scorecard(root)?;
    write_plan_after_scorecard_check(&checked_scorecard, results, output, selection)
}

fn write_plan_after_scorecard_check(
    checked_scorecard: &CheckedScorecard<'_>,
    results: &Path,
    output: &Path,
    selection: &CellSelection,
) -> Result<(RunMetadata, DagConfig), String> {
    let root = checked_scorecard.root;
    let PressureCells {
        selected: cells,
        unavailable,
        eligible_cells,
        preparation_by_test: all_preparations,
        cells_file_sha256,
    } = pressure_cells(root, selection)?;
    validate_guest_caps_against_selected_demand(&cells, selection)?;
    if checked_scorecard.enforce_host_capabilities {
        require_selected_kvm_capability(&cells, &strict_kvm_capability())?;
    }
    let preparation_by_test = if selection.uses_shared_preparation() {
        all_preparations
    } else {
        BTreeMap::new()
    };
    let includes_liteinst = cells.iter().any(|tracked| tracked.id.backend == "liteinst");
    let exact_cell = selection.is_exact().then(|| {
        (
            selection.mode.as_deref().expect("exact selection has mode"),
            selection
                .backend
                .as_deref()
                .expect("exact selection has backend"),
        )
    });
    let required_builds = required_build_tags(exact_cell, includes_liteinst);
    require_generated_node_count(cells.len(), selection.run_count(), preparation_by_test.len(), required_builds.len())?;
    let timeout_policy = PressureTimeoutPolicy::from_env()?;
    let selected_budgets = cells.iter().map(|tracked| &tracked.id)
        .chain(preparation_by_test.values())
        .map(|cell| (cell.test.clone(), cell.mode.clone(), cell.backend.clone()))
        .collect();
    let budgets = resolve_budgets(load_budgets(root)?, timeout_policy, &selected_budgets)?;
    let run_timeout_seconds = selection
        .run_timeout_seconds
        .unwrap_or(PRESSURE_RUN_TIMEOUT_SECONDS);
    require_cell_occupancy_fits(
        &cells,
        &budgets,
        selection.cell_timeout_seconds,
        run_timeout_seconds,
        selection.run_count(),
        selection.scheduler_jobs(),
        selection.manifest_guest_cap(),
        selection.kvm_guest_cap(),
    )?;
    fs::create_dir_all(results).map_err(|e| format!("cannot create {}: {e}", results.display()))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    let canonical_text = fs::read_to_string(root.join(PORTABLE_DAG))
        .map_err(|e| format!("cannot read {PORTABLE_DAG}: {e}"))?;
    let canonical =
        dag_from_json(&canonical_text).map_err(|e| format!("invalid {PORTABLE_DAG}: {e}"))?;
    let mut steps = Vec::new();
    for mut step in canonical.steps.iter().cloned() {
        let tag = step.tag();
        if required_builds.contains(tag.as_str()) {
            let marker = build_marker(results, &tag);
            let direct_backend_build = tag == "build.runtime_release"
                && exact_cell.is_some()
                && matches!(selection.backend.as_deref(), Some("ptrace" | "kvm"));
            let command = if direct_backend_build {
                "CARGO_BUILD_JOBS=8 cargo build --release --locked -p hermit --bin hermit".into()
            } else {
                step.cmd.clone()
            };
            step.cmd = format!(
                "mkdir -p {state}; if test -f {marker}; then exit 0; fi; ( {command} ) && printf 'ok\\n' > {marker}",
                state = shell_quote(&marker.parent().unwrap().to_string_lossy()),
                marker = shell_quote(&marker.to_string_lossy()),
            );
            // Preserve every canonical dependency. An unknown future
            // prerequisite refuses instead of silently shrinking or expanding
            // the explicitly selected build closure.
            retain_required_build_dependencies(&mut step, &required_builds)?;
            if direct_backend_build {
                step.timeout = 600;
                step.cpu_timeout = 1200;
                step.hint = ResourceHint {
                    resources: BTreeMap::from([("cargo_writer".into(), 1)]),
                    rss_baseline_bytes: Some(4_294_967_296),
                    hard_mem_max_bytes: Some(17_179_869_184),
                    classification: StepClass::CpuBound,
                    preferred_inner_jobs: Some(8),
                    ..ResourceHint::default()
                };
            }
            if step.cpu_timeout <= 0 {
                step.cpu_timeout = step.timeout * 2;
            }
            steps.push(step);
        }
    }
    if steps.len() != required_builds.len() {
        return Err(format!(
            "canonical build extraction found {} of {} required nodes",
            steps.len(),
            required_builds.len()
        ));
    }

    let sha = git_output(root, &["rev-parse", "HEAD"])?;
    let detcore_tree = git_output(root, &["rev-parse", "HEAD:detcore"])?;
    let build_root = results.join("build").join(&sha);
    let mut preparation_tags = BTreeMap::new();
    for (test, cell) in preparation_by_test {
        let budget = budgets
            .get(&(test.clone(), cell.mode.clone(), cell.backend.clone()))
            .ok_or_else(|| {
                format!(
                    "no manifest budget for {test}/{}/{}",
                    cell.mode, cell.backend
                )
            })?;
        let job = sanitize(&test);
        let tag = format!("prepare.{job}");
        let status_path = results.join("prepare").join(&job).join("status");
        let backend = if cell.backend == "native" {
            String::new()
        } else {
            format!(" --backend {}", shell_quote(&cell.backend))
        };
        let pressure_seconds = preparation_timeout(budget)?;
        let cmd = format!(
            "mkdir -p {status_dir}; if test -f {status}; then exit 0; fi; \
             printf '{incomplete}\\n' > {status}; status=0; \
             timeout --kill-after=10s {pressure_seconds}s env \
             E2E_RESULT_ROOT={results} E2E_BUILD_ROOT={build_root} \
             target/debug/test-harness build --include-manual --include-occasional \
             --test {test} --mode {mode}{backend} || status=$?; \
             printf '%s\\n' \"$status\" > {status}; exit 0",
            status_dir = shell_quote(&status_path.parent().unwrap().to_string_lossy()),
            results = shell_quote(&results.to_string_lossy()),
            build_root = shell_quote(&build_root.to_string_lossy()),
            test = shell_quote(&test),
            mode = shell_quote(&cell.mode),
            backend = backend,
            status = shell_quote(&status_path.to_string_lossy()),
            incomplete = INCOMPLETE_ATTEMPT_STATUS,
        );
        let wall = preparation_node_timeout(budget)?;
        let preparation_deps = if selection.is_exact() {
            selected_cell_dependencies(true, false, &cell.mode, &cell.backend, None)
        } else {
            vec!["setup.manifest_plan".into(), "build.e2e_artifact".into()]
        };
        steps.push(Step {
            group: "prepare".into(),
            job,
            desc: format!("Prepare selected-cell fixture {test}"),
            description: String::new(),
            cmd,
            cmdtype: CmdType::Unknown,
            manifest: None,
            integration_test_binaries: None,
            result_manifests: None,
            labels: Vec::new(),
            deps: preparation_deps,
            env: BTreeMap::new(),
            // `None` preserves the existing GLOBAL eager-exit behaviour, which is what
            // this graph had before the runner learned about fail-fast families.
            // Scoping the pressure graph into families is a separate decision.
            fail_fast_family: None,
            hint: ResourceHint {
                resources: BTreeMap::from([("cargo_writer".into(), 1)]),
                rss_baseline_bytes: Some(1_073_741_824),
                hard_mem_max_bytes: Some(PREPARATION_MEMORY_BYTES),
                classification: StepClass::CpuBound,
                ..ResourceHint::default()
            },
            networkonly: false,
            engine_only: false,
            timeout: wall,
            cpu_timeout: wall * 2,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            // Undeclared. These cells already serialize their cargo writes through
            // the `cargo_writer` resource cap above, and restating that as a write
            // domain would change how the scheduler treats them rather than leaving
            // the pressure DAG measuring what it measured before.
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
        });
        preparation_tags.insert(test, tag);
    }

    let mut cell_tags = Vec::new();
    let mut cell_timeouts = BTreeMap::new();
    for tracked in &cells {
        let cell = &tracked.id;
        let budget = budgets
            .get(&(cell.test.clone(), cell.mode.clone(), cell.backend.clone()))
            .ok_or_else(|| {
                format!(
                    "no manifest budget for {}/{}/{}",
                    cell.test, cell.mode, cell.backend
                )
            })?;
        for repetition in repetition_numbers(selection.repetitions) {
            let slug = cell_run_slug(cell, repetition);
            let evidence_run_id = cell_evidence_run_id(
                cell,
                repetition,
                selection.run_id_prefix.as_deref(),
            );
            let tag = format!("cell.{slug}");
            let cell_dir = results.join("cells").join(&slug);
            let result_file = cell_dir.join("results.jsonl");
            let result_in_progress = cell_dir.join("results.in-progress.jsonl");
            let junit = cell_dir.join("junit.xml");
            let junit_in_progress = cell_dir.join("junit.in-progress.xml");
            let status_file = cell_dir.join("harness-status");
            let (selector, backend) = if tracked.enabled {
                let backend = if cell.backend == "native" {
                    String::new()
                } else {
                    format!(" --backend {}", shell_quote(&cell.backend))
                };
                ("--include-manual", backend)
            } else {
                (
                    "--probe-disabled",
                    format!(" --backend {}", shell_quote(&cell.backend)),
                )
            };
            let preparation_guard = if selection.uses_shared_preparation() {
                let preparation_status = results
                    .join("prepare")
                    .join(sanitize(&cell.test))
                    .join("status");
                format!(
                    "if ! test \"$(cat {preparation_status} 2>/dev/null)\" = 0; then printf '{failed}\\n' > {status_file}; exit 0; fi; ",
                    preparation_status = shell_quote(&preparation_status.to_string_lossy()),
                    failed = PREPARATION_FAILED_STATUS,
                    status_file = shell_quote(&status_file.to_string_lossy()),
                )
            } else {
                String::new()
            };
            let harness = if selection.is_exact() {
                let prebuilt = if selection.uses_shared_preparation() {
                    " --prebuilt"
                } else {
                    ""
                };
                format!(
                    "HERMIT_BIN=\"$PWD/target/release/hermit\" target/debug/test-harness run {selector} --include-occasional{prebuilt} --test {test} --mode {mode}{backend} --results {result_file} --junit {junit}",
                    selector = selector,
                    prebuilt = prebuilt,
                    test = shell_quote(&cell.test),
                    mode = shell_quote(&cell.mode),
                    backend = backend,
                    result_file = shell_quote(&result_in_progress.to_string_lossy()),
                    junit = shell_quote(&junit_in_progress.to_string_lossy()),
                )
            } else {
                format!(
                    "./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run {selector} --include-occasional --prebuilt --test {test} --mode {mode}{backend} --results {result_file} --junit {junit}",
                    selector = selector,
                    test = shell_quote(&cell.test),
                    mode = shell_quote(&cell.mode),
                    backend = backend,
                    result_file = shell_quote(&result_in_progress.to_string_lossy()),
                    junit = shell_quote(&junit_in_progress.to_string_lossy()),
                )
            };
            let run_index = repetition.unwrap_or(0);
            let cmd = format!(
                "mkdir -p {cell_dir}; if test -f {status_file}; then exit 0; fi; \
             printf '{incomplete}\\n' > {status_file}; {preparation_guard}status=0; \
             env E2E_RESULT_ROOT={results} E2E_BUILD_ROOT={build_root} E2E_RUN_ID={run_id} \
             {run_index_env}={run_index} E2E_KEEP_VERIFY_LOGS=1 \
             {harness} \
             || status=$?; \
             if test -e {result_in_progress}; then mv -- {result_in_progress} {result_file} || status=$?; fi; \
             if test -e {junit_in_progress}; then mv -- {junit_in_progress} {junit} || status=$?; fi; \
             printf '%s\\n' \"$status\" > {status_file}; exit \"$status\"",
                cell_dir = shell_quote(&cell_dir.to_string_lossy()),
                results = shell_quote(&results.to_string_lossy()),
                build_root = shell_quote(&build_root.to_string_lossy()),
                run_id = shell_quote(&evidence_run_id),
                run_index_env = E2E_RUN_INDEX_ENV,
                run_index = run_index,
                harness = harness,
                result_in_progress = shell_quote(&result_in_progress.to_string_lossy()),
                result_file = shell_quote(&result_file.to_string_lossy()),
                junit_in_progress = shell_quote(&junit_in_progress.to_string_lossy()),
                junit = shell_quote(&junit.to_string_lossy()),
                status_file = shell_quote(&status_file.to_string_lossy()),
                incomplete = INCOMPLETE_ATTEMPT_STATUS,
                preparation_guard = preparation_guard,
            );
            let wall = pressure_timeout(budget, selection.cell_timeout_seconds)?;
            cell_timeouts.insert(tag.clone(), wall);
            // KVM's canonical privileged nodes are boxed at 16 GiB even when the
            // manifest cell itself is in the portable lane. Preserve that safety
            // boundary here; a 3 GiB generic portable cap kills the VM before its
            // compatibility result exists.
            let memory = cell_memory_bytes(cell);
            let mut resources = BTreeMap::from([("manifest_guest".into(), 1)]);
            if cell.backend == "kvm" {
                resources.insert("kvm_guest".into(), 1);
            }
            let deps = selected_cell_dependencies(
                selection.is_exact(),
                selection.uses_shared_preparation(),
                &cell.mode,
                &cell.backend,
                preparation_tags.get(&cell.test).map(String::as_str),
            );
            steps.push(Step {
                group: "cell".into(),
                job: slug,
                desc: if let Some(number) = repetition {
                    let population = population_label(selection.green, selection.probe_disabled);
                    format!(
                        "Repeat {population} cell {}/{}/{}@{} ({number}/{})",
                        cell.test,
                        cell.mode,
                        cell.backend,
                        cell.lane,
                        selection.run_count()
                    )
                } else {
                    let population = population_label(selection.green, selection.probe_disabled);
                    format!(
                        "Attempt {population} cell {}/{}/{}@{}",
                        cell.test, cell.mode, cell.backend, cell.lane
                    )
                },
                description: String::new(),
                cmd,
                cmdtype: CmdType::Unknown,
                manifest: None,
                integration_test_binaries: None,
                result_manifests: Some(vec![ResultManifest::StructuredTestResults(
                    StructuredTestResultsManifest::current(tag.clone()),
                )]),
                labels: Vec::new(),
                deps,
                // Requalification evidence must exercise the same hermetic
                // guest workdir contract as canonical validation. Otherwise a
                // pressure pass can promote a backend that the full run must
                // refuse before guest execution.
                env: BTreeMap::from([(
                    HERMETIC_TEST_WORKDIR_ENV.into(),
                    HERMETIC_TEST_WORKDIR.into(),
                )]),
                // `None` preserves the existing GLOBAL eager-exit behaviour, which is what
                // this graph had before the runner learned about fail-fast families.
                // Scoping the pressure graph into families is a separate decision.
                fail_fast_family: None,
                hint: ResourceHint {
                    resources,
                    rss_baseline_bytes: Some(memory / 3),
                    hard_mem_max_bytes: Some(memory),
                    classification: StepClass::LatencyBound,
                    ..ResourceHint::default()
                },
                networkonly: false,
                engine_only: false,
                timeout: wall,
                cpu_timeout: wall * 2,
                jobs_flag: None,
                jobs_env: None,
                skip_reason: None,
                write_domains: None,
                write_domain_guarantee: None,
                explains: Vec::new(),
            });
            cell_tags.push(tag);
        }
    }

    steps.push(Step {
        group: "pressure".into(),
        job: "summarize".into(),
        desc: if selection.repeats_cells() {
            format!(
                "Wait for every repeated {}-cell check before reading retained runner evidence",
                population_label(selection.green, selection.probe_disabled)
            )
        } else {
            format!(
                "Wait for every {}-cell attempt before reading retained runner evidence",
                population_label(selection.green, selection.probe_disabled)
            )
        },
        description: String::new(),
        cmd: "true".into(),
        cmdtype: CmdType::Unknown,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        labels: Vec::new(),
        deps: cell_tags,
        env: BTreeMap::new(),
        // `None` preserves the existing GLOBAL eager-exit behaviour, which is what
        // this graph had before the runner learned about fail-fast families.
        // Scoping the pressure graph into families is a separate decision.
        fail_fast_family: None,
        hint: ResourceHint {
            rss_baseline_bytes: Some(268_435_456),
            hard_mem_max_bytes: Some(1_073_741_824),
            classification: StepClass::Light,
            ..ResourceHint::default()
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 120,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
    });

    let max_timeout = steps.iter().map(|step| step.timeout).max().unwrap_or(120);
    let mut dag = canonical;
    dag.resource_caps = BTreeMap::from([
        ("cargo_writer".into(), 1),
        ("manifest_guest".into(), selection.manifest_guest_cap()),
        ("kvm_guest".into(), selection.kvm_guest_cap()),
    ]);
    dag.default_step_timeout = max_timeout;
    dag.default_step_cpu_timeout = max_timeout * 2;
    dag.steps = steps;
    let (
        manifest_guest_memory_budget_bytes,
        manifest_guest_memory_required_bytes,
        manifest_guest_max_safe_cap,
        kvm_guest_max_safe_cap,
    ) = validate_manifest_guest_memory(
        &dag,
        selection,
        checked_scorecard
            .memory_budget_override
            .or_else(observed_manifest_guest_memory_budget),
    )?;
    let expected_runs = cells.len().saturating_mul(selection.run_count());
    audit_dag(&dag, expected_runs, run_timeout_seconds, &cell_timeouts)?;
    let mut dag_text = dag_to_json(&dag);
    dag_text.push('\n');
    let reparsed = dag_from_json(&dag_text)
        .map_err(|e| format!("generated pressure DAG does not parse: {e}"))?;
    assert_plan_round_trip(&dag, &reparsed)?;
    audit_dag(
        &reparsed,
        expected_runs,
        run_timeout_seconds,
        &cell_timeouts,
    )?;
    let retained_output = results.join("dag.json");
    fs::write(&retained_output, &dag_text)
        .map_err(|e| format!("cannot write {}: {e}", retained_output.display()))?;
    if output != retained_output {
        return Err(format!(
            "pressure plan must be retained and executed at {}; refusing alternate output {}",
            retained_output.display(),
            output.display()
        ));
    }

    let selected_cells: Vec<_> = cells.into_iter().map(|cell| cell.id).collect();
    let selected_population_sha256 = selection
        .cells_file
        .as_ref()
        .map(|_| selected_population_sha256(&selected_cells))
        .transpose()?;
    let metadata = RunMetadata {
        schema: RUN_SCHEMA,
        run_id: results
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && *name != "." && *name != "..")
            .ok_or_else(|| format!("{} has no usable run id", results.display()))?
            .to_string(),
        hermit_sha: sha,
        detcore_tree,
        source_tree_dirty: worktree_dirty(root)?,
        run_timeout_seconds,
        timeout_policy: Some(timeout_policy),
        mode: selection.mode.clone(),
        test: selection.test.clone(),
        backend: selection.backend.clone(),
        cell_timeout_seconds: selection.cell_timeout_seconds,
        sample: selection.sample,
        seed: selection.seed,
        unavailable_cells: unavailable.len(),
        repetitions: selection.repetitions,
        run_id_prefix: selection.run_id_prefix.clone(),
        green: selection.green,
        probe_disabled: selection.probe_disabled,
        jobs: selection.scheduler_jobs(),
        manifest_guest_cap: selection.manifest_guest_cap(),
        manifest_guest_cap_explicit: selection.manifest_guest_cap.is_some(),
        kvm_guest_cap: selection.kvm_guest_cap(),
        kvm_guest_cap_explicit: selection.kvm_guest_cap.is_some(),
        manifest_guest_memory_budget_bytes,
        manifest_guest_memory_required_bytes: Some(manifest_guest_memory_required_bytes),
        manifest_guest_control_plane_headroom_bytes: Some(CONTROL_PLANE_HEADROOM_BYTES),
        manifest_guest_max_safe_cap,
        kvm_guest_max_safe_cap,
        eligible_cells,
        cells_file: selection
            .cells_file
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        cells_file_sha256,
        selected_population_sha256,
        cells: selected_cells,
    };
    let mut metadata_text = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("cannot serialize run metadata: {e}"))?;
    metadata_text.push('\n');
    fs::write(results.join("run.json"), metadata_text)
        .map_err(|e| format!("cannot write run metadata: {e}"))?;
    Ok((metadata, dag))
}

fn audit_dag(
    dag: &DagConfig,
    expected_cells: usize,
    run_timeout: i64,
    expected_cell_timeouts: &BTreeMap<String, i64>,
) -> Result<(), String> {
    let mut tags = BTreeSet::new();
    let mut deps = Vec::new();
    let mut cells = 0usize;
    let mut summaries = 0usize;
    for step in &dag.steps {
        let tag = step.tag();
        if !tags.insert(tag.clone()) {
            return Err(format!("generated DAG has duplicate tag {tag}"));
        }
        let timeout = step.timeout;
        let cpu_timeout = step.cpu_timeout;
        if timeout <= 0 || cpu_timeout <= 0 || timeout >= run_timeout {
            return Err(format!(
                "{tag} has invalid timeout ladder wall={timeout} cpu={cpu_timeout} run={run_timeout}"
            ));
        }
        if step.hint.hard_mem_max_bytes.unwrap_or(0) <= 0 {
            return Err(format!("{tag} has no hard memory cap"));
        }
        for (resource, demand) in &step.hint.resources {
            let capacity = dag.resource_caps.get(resource).copied().unwrap_or(0);
            if *demand <= 0 || capacity < *demand {
                return Err(format!(
                    "{tag} requests {demand} unit(s) of {resource}, but the DAG grants {capacity}"
                ));
            }
        }
        for dep in &step.deps {
            deps.push((tag.clone(), dep.clone()));
        }
        if step.group == "cell" {
            cells += 1;
            let expected_timeout = expected_cell_timeouts
                .get(&tag)
                .ok_or_else(|| format!("{tag} has no derived cell wall cap"))?;
            if timeout != *expected_timeout {
                return Err(format!(
                    "{tag} wall timeout {timeout}s does not equal its derived {expected_timeout}s cap"
                ));
            }
            let cmd = &step.cmd;
            let enabled_selector = cmd.contains("--include-manual");
            let disabled_selector = cmd.contains("--probe-disabled");
            let prepared_input = cmd.contains("--prebuilt")
                || cmd.contains("HERMIT_BIN=\"$PWD/target/release/hermit\"");
            if cmd.contains("timeout --kill-after=10s")
                || !cmd.contains("printf '125")
                || !cmd.contains("exit \"$status\"")
                || !cmd.contains("results.in-progress.jsonl")
                || !cmd.contains("mv --")
                || enabled_selector == disabled_selector
                || !prepared_input
                || !cmd.contains("--test")
                || !cmd.contains("--mode")
                || !cmd.contains("--results")
                || !cmd.contains("--junit")
            {
                return Err(format!(
                    "{tag} lost its runner-bounded exact-cell harness command"
                ));
            }
            if cmd.contains("--prebuilt")
                && (!cmd.contains("/prepare/")
                    || !cmd.contains("/status")
                    || !cmd.contains("printf '126"))
            {
                return Err(format!(
                    "{tag} can consume a prebuilt fixture without refusing failed preparation"
                ));
            }
        }
        if tag == "pressure.summarize" {
            summaries += 1;
        }
    }
    for (tag, dep) in deps {
        if !tags.contains(&dep) {
            return Err(format!("{tag} depends on absent step {dep}"));
        }
    }
    if cells != expected_cells || expected_cell_timeouts.len() != expected_cells || summaries != 1 {
        return Err(format!(
            "generated DAG shape mismatch: cells={cells}/{expected_cells}, timeout_caps={}/{expected_cells}, summaries={summaries}/1",
            expected_cell_timeouts.len()
        ));
    }
    Ok(())
}

/// Prove that the retained inspection JSON preserves the typed plan's commands,
/// dependencies, and effective containment. The pinned serializer
/// intentionally omits DagConfig's default step CPU/memory/core fields. That
/// is harmless only because every generated node declares wall, CPU, and hard
/// memory caps; compare their effective values here rather than assuming a
/// structural round trip. Execution uses the original `DagConfig`, never this
/// reparsed copy.
fn assert_plan_round_trip(expected: &DagConfig, actual: &DagConfig) -> Result<(), String> {
    if dag_to_json(expected) != dag_to_json(actual) {
        return Err("generated pressure DAG changed during typed JSON round trip".into());
    }
    if expected.resource_caps != actual.resource_caps {
        return Err("generated pressure DAG changed named resource capacities".into());
    }
    if expected.steps.len() != actual.steps.len() {
        return Err("generated pressure DAG changed its step count".into());
    }
    for (before, after) in expected.steps.iter().zip(&actual.steps) {
        let tag = before.tag();
        if tag != after.tag()
            || before.timeout != after.timeout
            || before.hint.hard_mem_max_bytes != after.hint.hard_mem_max_bytes
            || before.hint.resources != after.hint.resources
            // The platform multiplier is caller policy and is deliberately NOT
            // persisted with the graph, so the reparsed copy always carries the
            // default. Scaling both sides by the SAME multiplier keeps this a
            // comparison of the graph's caps; taking each config's own would make a
            // lane that sets a multiplier fail a round trip that did not change.
            || effective_cpu_timeout(
                before,
                expected.default_step_cpu_timeout,
                expected.cpu_timeout_multiplier,
            ) != effective_cpu_timeout(
                after,
                actual.default_step_cpu_timeout,
                expected.cpu_timeout_multiplier,
            )
            || effective_cpu_count(before, expected.default_step_cpu_count)
                != effective_cpu_count(after, actual.default_step_cpu_count)
        {
            return Err(format!(
                "generated pressure DAG changed effective caps or resource demand for {tag}"
            ));
        }
    }
    Ok(())
}

fn validate_run_contract(
    root: &Path,
    results: &Path,
    metadata: &RunMetadata,
    allow_dirty_exact_cell: bool,
) -> Result<BTreeMap<CellId, bool>, String> {
    let cells_file_fields = [
        metadata.cells_file.is_some(),
        metadata.cells_file_sha256.is_some(),
        metadata.selected_population_sha256.is_some(),
    ];
    if cells_file_fields.iter().any(|present| *present)
        && !cells_file_fields.iter().all(|present| *present)
    {
        return Err(
            "retained --cells-file run must record source path, file SHA-256, and selected-population SHA-256"
                .into(),
        );
    }
    if metadata.cells_file.is_some() && metadata.repetitions.is_none() {
        return Err("retained --cells-file run is not repeated".into());
    }
    for digest in [
        metadata.cells_file_sha256.as_deref(),
        metadata.selected_population_sha256.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !is_lower_sha256(digest) {
            return Err("retained --cells-file SHA-256 is malformed".into());
        }
    }
    if let Some(retained_digest) = &metadata.selected_population_sha256 {
        let actual_digest = selected_population_sha256(&metadata.cells)?;
        if actual_digest != *retained_digest {
            return Err(format!(
                "retained selected-cell population SHA-256 mismatch: recorded={retained_digest} actual={actual_digest}"
            ));
        }
    }
    if metadata.source_tree_dirty && !allow_dirty_exact_cell {
        return Err("pressure run metadata claims a dirty source tree".into());
    }
    if metadata.source_tree_dirty
        && (metadata.test.is_none()
            || metadata.mode.is_none()
            || metadata.backend.is_none()
            || metadata.sample.is_some()
            || metadata.seed.is_some())
    {
        return Err("dirty pressure results are accepted only for one exact cell".into());
    }
    if metadata.source_tree_dirty && metadata.repetitions.is_some() {
        return Err("repeated-cell results require a clean committed source tree".into());
    }
    if metadata.sample.is_some() != metadata.seed.is_some() {
        return Err("retained sampled run must record both --sample and its seed".into());
    }
    let detcore_tree = git_output(root, &["rev-parse", "HEAD:detcore"])?;
    if detcore_tree != metadata.detcore_tree {
        return Err(format!(
            "pressure run detcore tree is {}, checkout has {}",
            metadata.detcore_tree, detcore_tree
        ));
    }

    let selection = CellSelection {
        mode: metadata.mode.clone(),
        test: metadata.test.clone(),
        backend: metadata.backend.clone(),
        cell_timeout_seconds: metadata.cell_timeout_seconds,
        sample: metadata.sample,
        seed: metadata.seed,
        run_timeout_seconds: Some(metadata.run_timeout_seconds),
        repetitions: metadata.repetitions,
        run_id_prefix: metadata.run_id_prefix.clone(),
        green: metadata.green,
        probe_disabled: metadata.probe_disabled,
        jobs: Some(metadata.jobs),
        manifest_guest_cap: metadata
            .manifest_guest_cap_explicit
            .then_some(metadata.manifest_guest_cap),
        kvm_guest_cap: metadata
            .kvm_guest_cap_explicit
            .then_some(metadata.kvm_guest_cap),
        cells_file: None,
        retained_cells_file_cells: metadata
            .cells_file
            .as_ref()
            .map(|_| metadata.cells.clone()),
    };
    let pressure_cells = pressure_cells(root, &selection)?;
    validate_guest_caps_against_selected_demand(&pressure_cells.selected, &selection)?;
    if metadata.repetitions.is_some() && metadata.eligible_cells == 0 {
        return Err("repeated run metadata does not record its eligible-cell count".into());
    }
    if metadata.eligible_cells != 0 && metadata.eligible_cells != pressure_cells.eligible_cells {
        return Err(format!(
            "run metadata records {} eligible cell(s), current selection has {}",
            metadata.eligible_cells, pressure_cells.eligible_cells
        ));
    }
    if metadata.repetitions.is_some() {
        if pressure_cells.eligible_cells == 0 || metadata.cells.is_empty() {
            return Err("repeated run metadata records an empty selected population".into());
        }
        let expected_selected = metadata.sample.unwrap_or(pressure_cells.eligible_cells);
        if expected_selected == 0
            || expected_selected > pressure_cells.eligible_cells
            || metadata.cells.len() != expected_selected
        {
            return Err(format!(
                "repeated run metadata selects {} of {} eligible cell(s), but its sample requires {}",
                metadata.cells.len(),
                pressure_cells.eligible_cells,
                expected_selected
            ));
        }
    }
    if metadata.unavailable_cells != pressure_cells.unavailable.len() {
        let population = population_label(metadata.green, metadata.probe_disabled);
        return Err(format!(
            "run metadata records {} unavailable {population} cell(s), current manifest selection has {}",
            metadata.unavailable_cells,
            pressure_cells.unavailable.len()
        ));
    }
    let expected_cells = pressure_cells.selected;
    let mut expected = BTreeMap::new();
    for tracked in expected_cells {
        if expected.insert(tracked.id, tracked.enabled).is_some() {
            return Err(format!(
                "tracked scorecard contains a duplicate {}-cell identity",
                population_label(metadata.green, metadata.probe_disabled)
            ));
        }
    }
    let actual: BTreeSet<_> = metadata.cells.iter().cloned().collect();
    if actual.len() != metadata.cells.len() {
        return Err("run metadata contains a duplicate cell identity".into());
    }
    let expected_ids: BTreeSet<_> = expected.keys().cloned().collect();
    if actual != expected_ids {
        let missing = expected_ids
            .difference(&actual)
            .next()
            .map(display_id)
            .unwrap_or_else(|| "none".into());
        let extra = actual
            .difference(&expected_ids)
            .next()
            .map(display_id)
            .unwrap_or_else(|| "none".into());
        return Err(format!(
            "run metadata does not match the current selected population: expected={} actual={} first_missing={} first_extra={}",
            expected_ids.len(),
            actual.len(),
            missing,
            extra
        ));
    }

    let dag_path = results.join("dag.json");
    let dag_text = fs::read_to_string(&dag_path)
        .map_err(|e| format!("cannot read {}: {e}", dag_path.display()))?;
    let dag =
        dag_from_json(&dag_text).map_err(|e| format!("invalid {}: {e}", dag_path.display()))?;
    let retained_kvm_cap_matches = dag.resource_caps.get("kvm_guest")
        == Some(&metadata.kvm_guest_cap)
        || (!metadata.kvm_guest_cap_explicit
            && metadata.kvm_guest_cap == DEFAULT_KVM_GUEST_CAP
            && !dag.resource_caps.contains_key("kvm_guest"));
    if metadata.manifest_guest_cap <= 0
        || metadata.kvm_guest_cap <= 0
        || dag.resource_caps.get("manifest_guest") != Some(&metadata.manifest_guest_cap)
        || !retained_kvm_cap_matches
    {
        return Err(format!(
            "generated DAG guest caps do not match retained positive caps manifest={} kvm={}",
            metadata.manifest_guest_cap, metadata.kvm_guest_cap
        ));
    }
    if metadata.manifest_guest_cap != DEFAULT_MANIFEST_GUEST_CAP
        && !metadata.manifest_guest_cap_explicit
    {
        return Err("non-default retained manifest_guest cap is not marked explicit".into());
    }
    if metadata.kvm_guest_cap != DEFAULT_KVM_GUEST_CAP && !metadata.kvm_guest_cap_explicit {
        return Err("non-default retained KVM guest cap is not marked explicit".into());
    }
    let recomputed_memory = declared_memory_at_manifest_guest_cap(
        &dag,
        metadata.jobs,
        metadata.manifest_guest_cap,
        metadata.kvm_guest_cap,
    )?;
    if (metadata.manifest_guest_cap_explicit || metadata.kvm_guest_cap_explicit)
        && (metadata.manifest_guest_memory_budget_bytes.is_none()
            || metadata.manifest_guest_memory_required_bytes.is_none()
            || metadata.manifest_guest_control_plane_headroom_bytes
                != Some(CONTROL_PLANE_HEADROOM_BYTES)
            || metadata.manifest_guest_max_safe_cap.is_none()
            || metadata.kvm_guest_max_safe_cap.is_none())
    {
        return Err(
            "explicit retained guest cap lacks budget, requirement, headroom, or maximum-safe-cap evidence"
                .into(),
        );
    }
    if metadata
        .manifest_guest_control_plane_headroom_bytes
        .is_some_and(|recorded| recorded != CONTROL_PLANE_HEADROOM_BYTES)
    {
        return Err(format!(
            "retained manifest_guest control-plane headroom does not equal required {CONTROL_PLANE_HEADROOM_BYTES}"
        ));
    }
    if metadata
        .manifest_guest_memory_required_bytes
        .is_some_and(|recorded| recorded != recomputed_memory)
    {
        return Err(format!(
            "retained manifest_guest memory requirement does not match recomputed {recomputed_memory}"
        ));
    }
    if let Some(budget) = metadata.manifest_guest_memory_budget_bytes {
        let recomputed_max =
            max_safe_manifest_guest_effective_width(
                &dag,
                metadata.jobs,
                metadata.kvm_guest_cap,
                budget,
            )?;
        let recomputed_kvm_max = max_safe_kvm_guest_cap(
            &dag,
            metadata.jobs,
            metadata.manifest_guest_cap,
            budget,
        )?;
        if metadata
            .manifest_guest_max_safe_cap
            .is_some_and(|recorded| recorded != recomputed_max)
        {
            return Err(format!(
                "retained maximum-safe manifest guest width does not match recomputed {recomputed_max}"
            ));
        }
        if metadata
            .kvm_guest_max_safe_cap
            .is_some_and(|recorded| recorded != recomputed_kvm_max)
        {
            return Err(format!(
                "retained maximum-safe KVM guest cap does not match recomputed {recomputed_kvm_max}"
            ));
        }
        if (metadata.manifest_guest_cap_explicit || metadata.kvm_guest_cap_explicit)
            && (budget <= 0
                || recomputed_memory > budget
                || (metadata.manifest_guest_cap_explicit
                    && metadata.manifest_guest_cap > recomputed_max)
                || (metadata.kvm_guest_cap_explicit
                    && metadata.kvm_guest_cap > recomputed_kvm_max))
        {
            return Err(format!(
                "retained guest caps manifest={} kvm={} exceed maximum safe caps manifest={recomputed_max} kvm={recomputed_kvm_max} for recorded budget {budget}",
                metadata.manifest_guest_cap, metadata.kvm_guest_cap
            ));
        }
    } else if metadata.manifest_guest_cap_explicit || metadata.kvm_guest_cap_explicit {
        return Err("explicit retained guest caps have no observed memory budget".into());
    }
    let budgets = load_budgets(root)?;
    let budgets = match metadata.timeout_policy {
        Some(policy) => resolve_budgets(budgets, policy, &expected.keys()
            .map(|cell| (cell.test.clone(), cell.mode.clone(), cell.backend.clone()))
            .collect())?,
        None => budgets,
    };
    let mut expected_cell_timeouts = BTreeMap::new();
    for cell in expected.keys() {
        let budget = budgets
            .get(&(cell.test.clone(), cell.mode.clone(), cell.backend.clone()))
            .ok_or_else(|| {
                format!(
                    "no manifest budget for {}/{}/{}",
                    cell.test, cell.mode, cell.backend
                )
            })?;
        for repetition in repetition_numbers(metadata.repetitions) {
            expected_cell_timeouts.insert(
                format!("cell.{}", cell_run_slug(cell, repetition)),
                if metadata.timeout_policy.is_some() {
                    pressure_timeout(budget, metadata.cell_timeout_seconds)?
                } else {
                    legacy_pressure_timeout(budget, metadata.cell_timeout_seconds)?
                },
            );
        }
    }
    audit_dag(
        &dag,
        expected
            .len()
            .saturating_mul(metadata.repetitions.unwrap_or(1)),
        metadata.run_timeout_seconds,
        &expected_cell_timeouts,
    )?;
    let dag_cells: BTreeSet<_> = dag
        .steps
        .iter()
        .filter(|step| step.group == "cell")
        .map(|step| step.job.clone())
        .collect();
    let expected_jobs: BTreeSet<_> = expected
        .keys()
        .flat_map(|cell| {
            repetition_numbers(metadata.repetitions)
                .map(move |repetition| cell_run_slug(cell, repetition))
        })
        .collect();
    if dag_cells != expected_jobs {
        return Err("generated DAG cell identities do not match run metadata".into());
    }
    Ok(expected)
}

fn load_runner_evidence(
    results: &Path,
    hermit_sha: &str,
) -> Result<BTreeMap<String, RunnerEvidence>, String> {
    let profile_dir = results.join("runner-profile");
    let mut files = Vec::new();
    for entry in fs::read_dir(&profile_dir).map_err(|e| {
        format!(
            "cannot read retained runner profiles {}: {e}",
            profile_dir.display()
        )
    })? {
        let entry = entry.map_err(|e| format!("cannot read runner-profile entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("step_profiles_") && name.ends_with(".csv") {
            files.push(entry.path());
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!(
            "no retained per-step runner profile under {}",
            profile_dir.display()
        ));
    }

    let mut evidence = BTreeMap::<String, RunnerEvidence>::new();
    for path in files {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(&path)
            .map_err(|e| format!("cannot read {} as CSV: {e}", path.display()))?;
        let headers = reader
            .headers()
            .map_err(|e| format!("cannot read {} CSV header: {e}", path.display()))?
            .clone();
        let column = |name: &str| {
            headers
                .iter()
                .position(|candidate| candidate == name)
                .ok_or_else(|| format!("{} has no `{name}` column", path.display()))
        };
        let sha_column = column("git_sha")?;
        let step_column = column("step")?;
        let ok_column = column("ok")?;
        let timeout_column = column("timed_out")?;
        let cpu_timeout_column = column("cpu_timed_out")?;
        let oom_column = column("oom_kills")?;
        for (row_index, record) in reader.records().enumerate() {
            let fields = record
                .map_err(|e| format!("{}:{} is invalid CSV: {e}", path.display(), row_index + 2))?;
            if fields.get(sha_column) != Some(hermit_sha) {
                continue;
            }
            if fields.len() != headers.len() {
                return Err(format!(
                    "{}:{} has {} CSV fields, expected {}",
                    path.display(),
                    row_index + 2,
                    fields.len(),
                    headers.len()
                ));
            }
            let Some(step) = fields.get(step_column) else {
                continue;
            };
            let parse_bool = |column: usize, name: &str| -> Result<bool, String> {
                match fields.get(column).map(str::to_ascii_lowercase).as_deref() {
                    Some("true") => Ok(true),
                    Some("false") => Ok(false),
                    other => Err(format!(
                        "{}:{} has invalid {name} value {:?}",
                        path.display(),
                        row_index + 2,
                        other
                    )),
                }
            };
            let ok = parse_bool(ok_column, "ok")?;
            let timed_out = parse_bool(timeout_column, "timed_out")?
                || parse_bool(cpu_timeout_column, "cpu_timed_out")?;
            let oom_kills = fields
                .get(oom_column)
                .ok_or_else(|| {
                    format!(
                        "{}:{} has no oom_kills value",
                        path.display(),
                        row_index + 2
                    )
                })?
                .parse::<u64>()
                .map_err(|e| {
                    format!(
                        "{}:{} has invalid oom_kills: {e}",
                        path.display(),
                        row_index + 2
                    )
                })?;
            let row = evidence.entry(step.to_string()).or_default();
            row.seen = true;
            row.ok |= ok;
            row.timed_out |= timed_out;
            row.oom |= oom_kills > 0;
        }
    }
    if evidence.is_empty() {
        return Err(format!(
            "retained runner profiles contain no rows for Hermit {}",
            hermit_sha
        ));
    }
    Ok(evidence)
}

fn reason_reports_timeout(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| {
        reason.ends_with("(innermost E2E timeout: deadline reached (exit 124))")
            || reason.ends_with("(innermost E2E timeout: SIGKILL after 10 s grace (exit 137))")
    })
}

fn is_proven_timeout_attempt(runner: RunnerEvidence, harness_status: Option<i32>) -> bool {
    // safe-ci owns the cell wall clock. Its exact-SHA/exact-step timeout row
    // and the marker written before the harness starts are both required.
    // A child may exit 124 on its own, so a terminal status is never timeout
    // proof by itself.
    runner.seen
        && !runner.ok
        && runner.timed_out
        && !runner.oom
        && harness_status == Some(INCOMPLETE_ATTEMPT_STATUS)
}

fn is_proven_oom_attempt(runner: RunnerEvidence, harness_status: Option<i32>) -> bool {
    // The per-step row is already selected by exact source SHA and exact DAG
    // step name. The numeric marker proves that this cell began before its
    // cgroup reported an OOM kill; without both records, absence of terminal
    // artifacts is not evidence of a guest OOM.
    runner.seen
        && !runner.ok
        && runner.oom
        && !runner.timed_out
        && harness_status.is_some_and(|status| {
            !matches!(status, 0 | PREPARATION_FAILED_STATUS)
        })
}

fn runner_observed_terminal_attempt(runner: RunnerEvidence, harness_status: Option<i32>) -> bool {
    runner.seen
        && !runner.oom
        && !runner.timed_out
        && harness_status.is_some_and(|status| {
            !matches!(
                status,
                INCOMPLETE_ATTEMPT_STATUS | PREPARATION_FAILED_STATUS
            )
                && runner.ok == (status == 0)
        })
}

/// Attempts BEFORE the terminal one that located a divergence, earliest first.
///
/// ⚠️ A CELL THAT DIVERGED AND THEN PASSED ON RETRY STILL DIVERGED. The terminal
/// attempt is what the harness exit describes, so it is what the cell's result
/// reports, but reading only that attempt throws away a real observation -- and a
/// flake is precisely "diverged, then passed", which is the population the
/// standing retries-must-record-the-flake work exists to stop hiding.
///
/// These are emitted as additional summary rows carrying the same repetition and
/// their own framework-written attempt ordinal. The scorecard keys its duplicate
/// guard on all three values, so retries remain distinct without changing what a
/// repetition means.
fn earlier_attempts_that_located(rows: &[CellResult], terminal: u64) -> Vec<&CellResult> {
    let mut earlier: Vec<&CellResult> = rows
        .iter()
        .filter(|row| row.attempt < terminal)
        .filter(|row| {
            row.first_divergent_record.is_some()
                || row.first_divergent_syscall.is_some()
                || row.first_divergent_scheduler_turn.is_some()
                || row.first_divergent_virtual_nanoseconds.is_some()
                || row.first_divergent_left_message.is_some()
                || row.first_divergent_right_message.is_some()
        })
        .collect();
    earlier.sort_by_key(|row| row.attempt);
    earlier
}

fn result_row_identity_and_invocation_match(
    row: &CellResult,
    slug: &str,
    metadata: &RunMetadata,
    cell: &CellId,
    expected_required: bool,
) -> bool {
    let observed_backend = row.backend.as_deref().or_else(|| {
        if row.mode == "naked" {
            Some("native")
        } else {
            None
        }
    });
    let identity_matches = row.schema == CELL_RESULT_SCHEMA
        && row.run_id == slug
        && row.hermit_sha == metadata.hermit_sha
        && row.source_tree_dirty == metadata.source_tree_dirty
        && row.test == cell.test
        && row.category == cell.category
        && row.lane == cell.lane
        && row.mode == cell.mode
        && observed_backend == Some(cell.backend.as_str())
        && row.classification
            == if expected_required {
                "required"
            } else {
                "disabled"
            };
    let invocation_is_bound = !row.argv.is_empty()
        && !row.guest_argv.is_empty()
        && !row.env.is_empty()
        && !row.cwd.is_empty()
        && !row.shell_command.is_empty()
        && row.shell_command == literal_shell_command(&row.cwd, &row.env, &row.argv)
        && !row.attempts.is_empty()
        && invocation_attempts(row).is_ok()
        && row.attempts.first().is_some_and(|attempt| {
            attempt.argv == row.argv
                && attempt.guest_argv == row.guest_argv
                && attempt.env == row.env
                && attempt.cwd == row.cwd
                && attempt.shell_command == row.shell_command
        });
    identity_matches && invocation_is_bound
}

fn result_row_matches_cell(
    row: &CellResult,
    slug: &str,
    metadata: &RunMetadata,
    cell: &CellId,
    expected_required: bool,
    harness_status: Option<i32>,
) -> bool {
    let exit_matches = match row.outcome.as_str() {
        "PASS" => harness_status == Some(0),
        "FAIL" | "ERROR" => harness_status.is_some_and(|status| status != 0),
        _ => false,
    };
    result_row_identity_and_invocation_match(row, slug, metadata, cell, expected_required)
        && exit_matches
}

fn invocation_attempts(row: &CellResult) -> Result<&[AttemptResult], String> {
    let attempts = row.attempts.as_slice();
    if attempts.is_empty()
        || attempts.iter().any(|attempt| {
            attempt.index.trim().is_empty()
                || attempt.outcome.trim().is_empty()
                || attempt.argv.is_empty()
                || attempt.guest_argv.is_empty()
                || attempt.env.is_empty()
                || attempt.cwd.trim().is_empty()
                || attempt.shell_command.trim().is_empty()
                || attempt.shell_command
                    != literal_shell_command(&attempt.cwd, &attempt.env, &attempt.argv)
        })
    {
        return Err("result row has an incomplete attempt invocation".into());
    }
    Ok(attempts)
}

fn result_row_invocation(row: &CellResult) -> Result<JsonValue, String> {
    Ok(json!({
        "run_id": row.run_id,
        "argv": row.argv,
        "guest_argv": row.guest_argv,
        "env": row.env,
        "cwd": row.cwd,
        "shell_command": row.shell_command,
        "attempts": invocation_attempts(row)?,
    }))
}

fn classify_result(
    runner: RunnerEvidence,
    harness_status: Option<i32>,
    outcome: &str,
    row_valid: bool,
    reason: Option<&str>,
    mode: &str,
    verification_verdict: Option<&str>,
    verification_logs_retained: bool,
    verification_evidence_valid: bool,
) -> &'static str {
    if !runner.seen {
        "infrastructure-error"
    } else if let EnvBlockObservation::Denied(class) = runner.environmental_block_observation {
        // The retained node output is stronger evidence than any downstream
        // timeout, missing receipt, or assertion text caused by the denied
        // operation. Keep it out of every product-failure bucket. Only the
        // BPFJailer class is a sandbox denial; the other shared environmental
        // classes stay infrastructure errors rather than being mislabeled.
        if class == EnvBlockClass::BpfjailerBanner {
            "sandbox-denied"
        } else {
            "infrastructure-error"
        }
    } else if runner.oom {
        if is_proven_oom_attempt(runner, harness_status) && verification_evidence_valid {
            "oom"
        } else {
            "infrastructure-error"
        }
    } else if is_proven_timeout_attempt(runner, harness_status) {
        if verification_evidence_valid {
            "timeout"
        } else {
            "infrastructure-error"
        }
    } else if reason_reports_timeout(reason) {
        // The harness reached its own inner deadline and published a terminal
        // row. The missing verify/replay report is caused by that measured
        // timeout; it is not evidence that the attempt failed to launch.
        "timeout"
    } else if runner.timed_out
        || !verification_evidence_valid
        // Merged with the arm above rather than left adjacent to it: both yield
        // the same bucket, so ordering between them is unobservable, and once
        // the timeout arm moved ahead of them they became neighbours with
        // identical blocks.
        || (mode == "verify"
            && matches!(verification_verdict, Some("matched" | "diverged"))
            && !verification_logs_retained)
        || (row_valid && verification_verdict == Some("infrastructure_error"))
    {
        "infrastructure-error"
    } else if row_valid && mode == "verify" && verification_verdict == Some("diverged") {
        "determinism-failure"
    } else if row_valid && mode == "replay" && verification_verdict == Some("diverged") {
        "replay-failure"
    } else if outcome == "PASS"
        && row_valid
        && (!matches!(mode, "verify" | "replay") || verification_verdict == Some("matched"))
    {
        "pass"
    } else if row_valid && harness_status.is_some_and(|status| status != 0) {
        "crash-error"
    } else {
        "infrastructure-error"
    }
}

fn reconcile_recorded_result(
    recorded_result: Option<ObservedResult>,
    failure_class: Option<FailureClass>,
    derived_result: &'static str,
) -> Result<&'static str, String> {
    let Some(recorded_result) = recorded_result else {
        return Ok(derived_result);
    };
    if failure_class != recorded_result.failure_class() {
        return Err(format!(
            "framework result {} carries failure_class {:?}, expected {:?}",
            recorded_result.as_str(),
            failure_class,
            recorded_result.failure_class()
        ));
    }
    if matches!(
        recorded_result,
        ObservedResult::SandboxDenied | ObservedResult::InfrastructureError
    ) {
        // The framework owns the exact captured attempt output and serialized
        // this non-product result beside the checkout SHA. The outer retained
        // runner log remains corroborating evidence, not a second authority
        // that reconstructs the result from human-readable output.
        return Ok(recorded_result.as_str());
    }
    if recorded_result.as_str() != derived_result {
        return Err(format!(
            "framework result {} disagrees with pressure consistency check {derived_result}",
            recorded_result.as_str()
        ));
    }
    Ok(recorded_result.as_str())
}

fn repeated_result_description(
    terminal_passes: usize,
    clean_passes: usize,
    infrastructure_errors: usize,
    retried: usize,
    total: usize,
) -> &'static str {
    if total == 0 || infrastructure_errors > 0 {
        "incomplete"
    } else if clean_passes == total && retried == 0 {
        "passed every repetition"
    } else if terminal_passes == 0 {
        "failed every repetition"
    } else {
        "flaky"
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct RepeatedOutcomeCounts {
    expected_repetitions: usize,
    observed_repetitions: usize,
    qualifying_passes: usize,
    clean_passes: usize,
    terminal_passes: usize,
    product_failures: usize,
    infrastructure_failures: usize,
    prerequisite_failures: usize,
    no_results: usize,
    mixed_repetitions: usize,
    missing_repetitions: usize,
    unknown_history_repetitions: usize,
    retried_repetitions: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PressureSampleClassification {
    PromotionCandidate,
    Intermittent,
    ConfirmedFailing,
    InfrastructureFailure,
    PrerequisiteFailure,
    NoResult,
    Incomplete,
}

impl PressureSampleClassification {
    fn as_str(self) -> &'static str {
        match self {
            Self::PromotionCandidate => "promotion-candidate",
            Self::Intermittent => "intermittent",
            Self::ConfirmedFailing => "confirmed-failing",
            Self::InfrastructureFailure => "infrastructure-failure",
            Self::PrerequisiteFailure => "prerequisite-failure",
            Self::NoResult => "no-result",
            Self::Incomplete => "incomplete",
        }
    }
}

fn classify_pressure_sample(counts: RepeatedOutcomeCounts) -> PressureSampleClassification {
    let accounted = counts
        .terminal_passes
        .saturating_add(counts.product_failures)
        .saturating_add(counts.infrastructure_failures)
        .saturating_add(counts.prerequisite_failures)
        .saturating_add(counts.no_results)
        .saturating_add(counts.mixed_repetitions);
    if counts.expected_repetitions != PROMOTION_REPETITIONS
        || counts.observed_repetitions != counts.expected_repetitions
        || counts.missing_repetitions != 0
        || counts.unknown_history_repetitions != 0
        || accounted != counts.observed_repetitions
        || counts.qualifying_passes > counts.terminal_passes
        || counts.retried_repetitions > counts.observed_repetitions
    {
        return PressureSampleClassification::Incomplete;
    }
    if counts.qualifying_passes == PROMOTION_REPETITIONS
        && counts.terminal_passes == PROMOTION_REPETITIONS
        && counts.retried_repetitions == 0
        && counts.product_failures == 0
        && counts.infrastructure_failures == 0
        && counts.prerequisite_failures == 0
        && counts.no_results == 0
    {
        PressureSampleClassification::PromotionCandidate
    } else if counts.terminal_passes > 0 {
        PressureSampleClassification::Intermittent
    } else if counts.product_failures == PROMOTION_REPETITIONS {
        PressureSampleClassification::ConfirmedFailing
    } else if counts.infrastructure_failures == PROMOTION_REPETITIONS {
        PressureSampleClassification::InfrastructureFailure
    } else if counts.prerequisite_failures == PROMOTION_REPETITIONS {
        PressureSampleClassification::PrerequisiteFailure
    } else if counts.no_results == PROMOTION_REPETITIONS {
        PressureSampleClassification::NoResult
    } else {
        PressureSampleClassification::Incomplete
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RepetitionClassification {
    ProductFailure,
    InfrastructureFailure,
    PrerequisiteFailure,
    NoResult,
    Mixed,
    Missing,
}

#[derive(Debug, Deserialize)]
struct HarnessSummary {
    schema: u64,
    cells: usize,
    passed: usize,
    failed: usize,
    errors: usize,
    host_inapplicable: usize,
    host_inapplicable_cells: Vec<HostInapplicableCell>,
}

#[derive(Debug, Deserialize)]
struct HostInapplicableCell {
    test: String,
    mode: String,
    backend: Option<String>,
    reason: Option<String>,
}

fn retained_host_inapplicable(cell_dir: &Path, cell: &CellId) -> Result<bool, String> {
    let path = cell_dir.join("summary.json");
    if !path.is_file() {
        return Ok(false);
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read harness summary {}: {error}", path.display()))?;
    let summary: HarnessSummary = serde_json::from_str(&text)
        .map_err(|error| format!("invalid harness summary {}: {error}", path.display()))?;
    if summary.schema != 1 {
        return Err(format!(
            "unsupported harness summary schema {} in {}",
            summary.schema,
            path.display()
        ));
    }
    if summary.host_inapplicable == 0 {
        return Ok(false);
    }
    let observed = summary.host_inapplicable_cells.first();
    let observed_backend = observed.and_then(|entry| {
        entry.backend.as_deref().or_else(|| {
            if entry.mode == "naked" {
                Some("native")
            } else {
                None
            }
        })
    });
    if summary.cells != 1
        || summary.passed != 0
        || summary.failed != 0
        || summary.errors != 0
        || summary.host_inapplicable != 1
        || summary.host_inapplicable_cells.len() != 1
        || !observed.is_some_and(|entry| {
            entry.test == cell.test
                && entry.mode == cell.mode
                && observed_backend == Some(cell.backend.as_str())
                && entry.reason.as_deref().is_some_and(|reason| !reason.trim().is_empty())
        })
    {
        return Err(format!(
            "harness summary {} does not prove exactly one host-inapplicable selected cell {}/{}/{}",
            path.display(),
            cell.test,
            cell.mode,
            cell.backend
        ));
    }
    Ok(true)
}

fn classify_nonpassing_repetition(
    result: &str,
    result_rows: &[CellResult],
    row_valid: bool,
    evidence_valid: bool,
    rejected_result_history: bool,
    proven_timeout: bool,
    proven_oom: bool,
    retained_prerequisite: bool,
) -> RepetitionClassification {
    if retained_prerequisite {
        return if !row_valid
            && evidence_valid
            && result_rows.is_empty()
            && !proven_timeout
            && !proven_oom
        {
            RepetitionClassification::PrerequisiteFailure
        } else {
            RepetitionClassification::Mixed
        };
    }
    if !row_valid && rejected_result_history && result_rows.is_empty() {
        return RepetitionClassification::Missing;
    }
    if !row_valid && !proven_timeout && !proven_oom {
        return RepetitionClassification::Missing;
    }
    if (proven_timeout || proven_oom) && !row_valid {
        return if result_rows.is_empty() {
            RepetitionClassification::NoResult
        } else {
            RepetitionClassification::Mixed
        };
    }
    if row_valid && (evidence_valid || proven_timeout || proven_oom) && !result_rows.is_empty() {
        let pass = result_rows
            .iter()
            .any(|row| row.result == Some(ObservedResult::Pass));
        let product = result_rows
            .iter()
            .any(|row| row.failure_class == Some(FailureClass::ProductFailure));
        let infrastructure = result_rows.iter().any(|row| {
            row.failure_class == Some(FailureClass::UnderstoodInfrastructureFailure)
        });
        let prerequisite = result_rows.iter().any(|row| {
            row.failure_class == Some(FailureClass::UnderstoodPrerequisiteFailure)
        });
        let no_result = proven_timeout
            || proven_oom
            || result_rows
            .iter()
            .any(|row| row.failure_class == Some(FailureClass::NoResult));
        let untyped = result_rows
            .iter()
            .any(|row| row.result.is_none() && row.failure_class.is_none());
        let categories = usize::from(product)
            + usize::from(infrastructure)
            + usize::from(prerequisite)
            + usize::from(no_result)
            + usize::from(pass)
            + usize::from(untyped);
        return match (
            categories,
            pass,
            product,
            infrastructure,
            prerequisite,
            no_result,
        ) {
            (1, false, true, false, false, false) => {
                RepetitionClassification::ProductFailure
            }
            (1, false, false, true, false, false) => {
                RepetitionClassification::InfrastructureFailure
            }
            (1, false, false, false, true, false) => {
                RepetitionClassification::PrerequisiteFailure
            }
            (1, false, false, false, false, true) => RepetitionClassification::NoResult,
            _ => RepetitionClassification::Mixed,
        };
    }
    if matches!(result, "timeout" | "oom") {
        RepetitionClassification::NoResult
    } else {
        RepetitionClassification::InfrastructureFailure
    }
}

fn repeated_batch_result_description(
    _terminal_passes: usize,
    clean_passes: usize,
    infrastructure_errors: usize,
    retried: usize,
    total: usize,
) -> &'static str {
    if total == 0 || infrastructure_errors > 0 {
        "incomplete"
    } else if clean_passes == total && retried == 0 {
        "passed every repeated check"
    } else {
        "one or more repeated checks failed or required a retry"
    }
}

fn top_level_repeated_result_description(
    metadata: &RunMetadata,
    terminal_passes: usize,
    clean_passes: usize,
    infrastructure_errors: usize,
    retried: usize,
    total: usize,
) -> &'static str {
    if metadata.is_exact() {
        repeated_result_description(
            terminal_passes,
            clean_passes,
            infrastructure_errors,
            retried,
            total,
        )
    } else {
        repeated_batch_result_description(
            terminal_passes,
            clean_passes,
            infrastructure_errors,
            retried,
            total,
        )
    }
}

fn retained_attempt_count(
    result_rows: &[CellResult],
    slug: &str,
    metadata: &RunMetadata,
    cell: &CellId,
    expected_required: bool,
    runner: RunnerEvidence,
    harness_status: Option<i32>,
) -> Result<usize, String> {
    if !result_rows.is_empty()
        && result_rows.iter().all(|row| {
            result_row_identity_and_invocation_match(row, slug, metadata, cell, expected_required)
        })
    {
        let (_, attempts) = cell_result_and_attempts_after_retries(result_rows)?;
        return usize::try_from(attempts)
            .map_err(|_| format!("result attempt count {attempts} does not fit usize"));
    }
    Ok(usize::from(
        runner_observed_terminal_attempt(runner, harness_status)
            || is_proven_timeout_attempt(runner, harness_status)
            || is_proven_oom_attempt(runner, harness_status),
    ))
}

fn repetition_passed_cleanly(terminal_result: &str, result_rows: &[CellResult]) -> bool {
    terminal_result == "pass"
        && !result_rows.is_empty()
        && result_rows.iter().all(|row| row.outcome == "PASS")
}

/// Qualifying a sample is stricter than the retained legacy clean-pass count.
/// One framework attempt may contain several declared seeds or subruns; every
/// one must pass, and none may be an error or a timed-out observation.
fn qualifying_subruns(mode: &str, attempts: &[AttemptResult]) -> bool {
    let mut indices = BTreeSet::new();
    !attempts.is_empty()
        && attempts.iter().all(|attempt| {
            !attempt.index.trim().is_empty()
                && indices.insert(attempt.index.as_str())
                && retained_pressure_attempt(mode, attempt).is_ok_and(|retained| {
                    retained.outcome == "PASS"
                        && retained.error_kind.is_none()
                        && !retained.timed_out
                        && inner_pressure_category(&retained).is_none()
                })
        })
}

fn canonical_pressure_comparison(mode: &str, report: &VerificationReport) -> bool {
    if report.require_canonical_comparison().is_err() {
        return false;
    }
    let Some(comparison) = &report.comparison else {
        return false;
    };
    comparison.display_name.as_deref() == Some("BitwiseInfoV1")
        && comparison.compare_io_buffers == Some(true)
        && comparison.log_scope
            == Some(hermit_manifest_plan::canonical_verdict::ComparedLogScope::Info)
        && comparison.virtualize_time == Some(mode != "replay")
        && comparison.strip_lines == Some(false)
        && comparison.canonicalize_addresses == Some(true)
        && comparison.full_trace == Some(true)
        && comparison.exact_remainder == Some(true)
        && comparison
            .stripped_prefixes
            .as_deref()
            .is_some_and(|values| values == ["real-wall-clock-prefix/v1"])
        && comparison
            .canonicalizations
            .as_deref()
            .is_some_and(|values| values == ["host-address-to-first-appearance-ordinal/v1"])
        && comparison.ignore_lines == Some(false)
        && comparison.skip_commit == Some(false)
        && comparison.skip_detlog == Some(false)
}

fn retained_pressure_attempt(
    mode: &str,
    attempt: &AttemptResult,
) -> Result<SeriesPressureAttempt, String> {
    // The runner reads comparison reports only for these modes. Its generic
    // prelaunch timeout also retains a NotRun stamp for native/custom modes;
    // that stamp is raw evidence, not a comparison performed by those modes.
    let comparison = match (
        matches!(mode, "verify" | "replay" | "chaos"),
        &attempt.verification_report,
    ) {
        (false, _) => None,
        (true, None) => {
            if attempt.verification_report_sha256.is_some() {
                return Err("inner invocation has a report digest without report bytes".into());
            }
            None
        }
        (true, Some(raw)) => {
            let digest = format!("{:x}", sha2::Sha256::digest(raw.as_bytes()));
            if attempt.verification_report_sha256.as_deref() != Some(digest.as_str()) {
                return Err(
                    "inner verification report digest differs from its retained bytes".into(),
                );
            }
            let value =
                serde_json::from_str::<JsonValue>(raw).map_err(|error| error.to_string())?;
            let report = VerificationReport::from_current_json_value(value)?;
            if report.guest_exit_code.is_some_and(|status| status < 0)
                || report.guest_signal.is_some_and(|signal| signal <= 0)
                || (report.guest_exit_code.is_some() && report.guest_signal.is_some())
            {
                return Err("inner report has an invalid guest process disposition".into());
            }
            let no_result_kind = match report.verdict {
                Verdict::Matched | Verdict::Diverged => {
                    let matched = report.verdict == Verdict::Matched;
                    if report.verified != matched
                        || report.bitwise_parity != matched
                        || report.no_result_reason.is_some()
                        || !report
                            .compared_log_messages
                            .is_some_and(|counts| counts.left > 0 && counts.right > 0)
                    {
                        return Err("inner comparison report contradicts its verdict or lacks positive counts".into());
                    }
                    None
                }
                Verdict::InfrastructureError => {
                    if report.verified || report.bitwise_parity || report.no_result_reason.is_some()
                    {
                        return Err("inner infrastructure report contradicts its verdict".into());
                    }
                    None
                }
                Verdict::NoResult => {
                    if report.verified
                        || report.bitwise_parity
                        || report.comparison.is_some()
                        || report.compared_log_messages.is_some()
                        || report.dbt_counted_branches.is_some()
                        || report.first_divergent_scheduler_turn.is_some()
                        || report.first_divergent_virtual_nanoseconds.is_some()
                        || report.first_divergent_record.is_some()
                        || report.first_divergent_syscall.is_some()
                        || report.first_divergent_left_message.is_some()
                        || report.first_divergent_right_message.is_some()
                        || attempt.first_divergent_scheduler_turn.is_some()
                        || attempt.first_divergent_virtual_nanoseconds.is_some()
                        || attempt.first_divergent_record.is_some()
                        || attempt.first_divergent_syscall.is_some()
                        || attempt.first_divergent_left_message.is_some()
                        || attempt.first_divergent_right_message.is_some()
                    {
                        return Err(
                            "inner no-result report carries contradictory comparison evidence"
                                .into(),
                        );
                    }
                    Some(match &report.no_result_reason {
                        Some(NoResultReason::NotRun) => {
                            if report.guest_exit_code.is_some() || report.guest_signal.is_some() {
                                return Err(
                                    "inner NotRun report invents a guest disposition".into()
                                );
                            }
                            SeriesNoVerdictKind::NotRun
                        }
                        Some(NoResultReason::FirstRunRejected {
                            exit_code, signal, ..
                        }) => {
                            if exit_code.is_some() == signal.is_some()
                                || exit_code.is_some_and(|status| status < 0)
                                || signal.is_some_and(|signal| signal <= 0)
                                || report.guest_exit_code != *exit_code
                                || report.guest_signal != *signal
                            {
                                return Err("inner FirstRunRejected report has inconsistent guest disposition".into());
                            }
                            SeriesNoVerdictKind::FirstRunRejected
                        }
                        None => return Err("inner no-result report omitted its reason".into()),
                    })
                }
            };
            Some(SeriesPressureComparison {
                verdict: report.verdict,
                canonical: matches!(report.verdict, Verdict::Matched | Verdict::Diverged)
                    && canonical_pressure_comparison(mode, &report)
                    && (report.verdict != Verdict::Matched
                        || report
                            .compared_log_messages
                            .is_some_and(|counts| counts.left == counts.right)),
                report_sha256: digest,
                no_result_kind,
            })
        }
    };
    let retained = SeriesPressureAttempt {
        index: attempt.index.clone(),
        outcome: attempt.outcome.clone(),
        error_kind: attempt.error_kind.clone(),
        status: attempt.status,
        signal: attempt.signal,
        timed_out: attempt.timed_out,
        comparison,
    };
    retained.validate_for_mode(mode)?;
    Ok(retained)
}

fn retained_typed_no_comparison(cell: &CellId, artifact_dir: &Path, rows: &[CellResult]) -> bool {
    if !matches!(cell.mode.as_str(), "verify" | "replay") {
        return false;
    }
    let Ok(row) = cell_result_after_retries(rows) else {
        return false;
    };
    let Some(attempt) = row.attempts.first() else {
        return false;
    };
    let Ok(retained) = retained_pressure_attempt(&cell.mode, attempt) else {
        return false;
    };
    if !retained
        .comparison
        .as_ref()
        .is_some_and(|comparison| comparison.verdict == Verdict::NoResult)
    {
        return false;
    }
    let Ok(bytes) = fs::read(verification_report_path(artifact_dir)) else {
        return false;
    };
    attempt
        .verification_report
        .as_ref()
        .is_some_and(|report| bytes == report.as_bytes())
}

fn inner_pressure_category(attempt: &SeriesPressureAttempt) -> Option<RepetitionClassification> {
    // This precedence is the runner's exact non_product_failure_class mapping.
    match attempt.error_kind.as_deref() {
        Some("guest-launch-refused" | "backend-unavailable") => {
            return Some(RepetitionClassification::PrerequisiteFailure);
        }
        Some("infrastructure" | "result-publication") => {
            return Some(RepetitionClassification::InfrastructureFailure);
        }
        Some("incomplete-verification-evidence" | "invalid-backend-evidence") => {
            return Some(RepetitionClassification::NoResult);
        }
        _ => {}
    }
    if attempt.timed_out {
        return Some(RepetitionClassification::NoResult);
    }
    if let Some(comparison) = &attempt.comparison {
        return match comparison.verdict {
            Verdict::Matched if comparison.canonical => None,
            Verdict::Diverged if comparison.canonical => {
                Some(RepetitionClassification::ProductFailure)
            }
            Verdict::Matched | Verdict::Diverged => Some(RepetitionClassification::NoResult),
            Verdict::InfrastructureError => Some(RepetitionClassification::InfrastructureFailure),
            Verdict::NoResult
                if comparison.no_result_kind == Some(SeriesNoVerdictKind::FirstRunRejected) =>
            {
                Some(RepetitionClassification::ProductFailure)
            }
            Verdict::NoResult => Some(RepetitionClassification::NoResult),
        };
    }
    match attempt.outcome.as_str() {
        "PASS" => None,
        "FAIL" => Some(RepetitionClassification::ProductFailure),
        _ => Some(RepetitionClassification::NoResult),
    }
}

fn inner_pressure_history(
    rows: &[CellResult],
) -> Result<BTreeSet<RepetitionClassification>, String> {
    // Keep the shared maximum, contiguous ordinals, terminal-PASS refusal and
    // framework-selected outcome. Inner declared subruns do not add retries.
    cell_result_after_retries(rows)?;
    let mut categories = BTreeSet::new();
    for row in rows {
        if row.attempts.is_empty() {
            return Err(format!(
                "outer attempt {} has no retained inner history",
                row.attempt
            ));
        }
        let mut indices = BTreeSet::new();
        for attempt in &row.attempts {
            if attempt.index.trim().is_empty() || !indices.insert(&attempt.index) {
                return Err(format!(
                    "outer attempt {} has empty or duplicate inner indices",
                    row.attempt
                ));
            }
            let retained = retained_pressure_attempt(&row.mode, attempt)?;
            if let Some(category) = inner_pressure_category(&retained) {
                categories.insert(category);
            }
        }
    }
    Ok(categories)
}

fn fold_pressure_history(
    outer: RepetitionClassification,
    inner: &BTreeSet<RepetitionClassification>,
) -> RepetitionClassification {
    if matches!(
        outer,
        RepetitionClassification::Missing | RepetitionClassification::Mixed
    ) {
        return outer;
    }
    if inner.iter().any(|category| *category != outer) {
        RepetitionClassification::Mixed
    } else {
        outer
    }
}

fn repetition_qualifies_for_promotion(terminal_result: &str, rows: &[CellResult]) -> bool {
    repetition_passed_cleanly(terminal_result, rows)
        && rows.len() == 1
        && rows[0].attempt == 1
        && rows[0].result == Some(ObservedResult::Pass)
        && rows[0].failure_class.is_none()
        && !rows[0].source_tree_dirty
        && qualifying_subruns(&rows[0].mode, &rows[0].attempts)
}

fn repeated_run_has_unacceptable_product_result(
    repetitions: Option<usize>,
    repeated_red: bool,
    clean_passes: usize,
    retried: usize,
    total: usize,
) -> bool {
    repetitions.is_some()
        && !repeated_red
        && (total == 0 || clean_passes != total || retried > 0)
}

fn repeated_cell_summary(
    cell: &CellId,
    counts: RepeatedOutcomeCounts,
    result: &str,
) -> JsonValue {
    let classification = classify_pressure_sample(counts);
    json!({
        "cell": cell,
        "passes": counts.terminal_passes,
        "clean_passes": counts.clean_passes,
        "retried_repetitions": counts.retried_repetitions,
        "total": counts.expected_repetitions,
        "result": result,
        "classification": classification,
        "promotion_candidate": classification == PressureSampleClassification::PromotionCandidate,
        "expected_repetitions": counts.expected_repetitions,
        "observed_repetitions": counts.observed_repetitions,
        "qualifying_passes": counts.qualifying_passes,
        "terminal_product_failures": counts.product_failures,
        "infrastructure_failures": counts.infrastructure_failures,
        "prerequisite_failures": counts.prerequisite_failures,
        "no_results": counts.no_results,
        "mixed_repetitions": counts.mixed_repetitions,
        "missing_repetitions": counts.missing_repetitions,
        "unknown_history_repetitions": counts.unknown_history_repetitions,
    })
}

fn verify_repetition_summary_json(
    summary: &JsonValue,
    attempted: usize,
    retried_repetitions: usize,
) -> Result<(), String> {
    if summary
        .get("probe_disabled")
        .and_then(JsonValue::as_bool)
        .is_none()
    {
        return Err("summary JSON lost its disabled-population identity".into());
    }
    if summary.get("attempted").and_then(JsonValue::as_u64) != Some(attempted as u64) {
        return Err("summary JSON lost the retained harness-attempt count".into());
    }
    if summary
        .get("retried_repetitions")
        .and_then(JsonValue::as_u64)
        != Some(retried_repetitions as u64)
    {
        return Err("summary JSON lost the retried-repetition count".into());
    }
    let repeated_cells = summary
        .get("repeated_cells")
        .and_then(JsonValue::as_array)
        .ok_or("summary JSON lost its repeated-cell array")?;
    for cell in repeated_cells {
        let terminal_passes = cell.get("passes").and_then(JsonValue::as_u64);
        let clean_passes = cell.get("clean_passes").and_then(JsonValue::as_u64);
        let retried = cell
            .get("retried_repetitions")
            .and_then(JsonValue::as_u64);
        let total = cell.get("total").and_then(JsonValue::as_u64);
        let expected = cell
            .get("expected_repetitions")
            .and_then(JsonValue::as_u64);
        let observed = cell
            .get("observed_repetitions")
            .and_then(JsonValue::as_u64);
        let qualifying = cell
            .get("qualifying_passes")
            .and_then(JsonValue::as_u64);
        let product_failures = cell
            .get("terminal_product_failures")
            .and_then(JsonValue::as_u64);
        let infrastructure_failures = cell
            .get("infrastructure_failures")
            .and_then(JsonValue::as_u64);
        let prerequisite_failures = cell
            .get("prerequisite_failures")
            .and_then(JsonValue::as_u64);
        let no_results = cell.get("no_results").and_then(JsonValue::as_u64);
        let mixed = cell
            .get("mixed_repetitions")
            .and_then(JsonValue::as_u64);
        let missing = cell
            .get("missing_repetitions")
            .and_then(JsonValue::as_u64);
        let unknown_history = cell.get("unknown_history_repetitions").and_then(JsonValue::as_u64);
        let classification = cell.get("classification").and_then(JsonValue::as_str);
        let promotion_candidate = cell
            .get("promotion_candidate")
            .and_then(JsonValue::as_bool);
        if terminal_passes.is_none()
            || clean_passes.is_none()
            || retried.is_none()
            || total.is_none()
            || expected.is_none()
            || observed.is_none()
            || qualifying.is_none()
            || product_failures.is_none()
            || infrastructure_failures.is_none()
            || prerequisite_failures.is_none()
            || no_results.is_none()
            || mixed.is_none()
            || missing.is_none()
            || unknown_history.is_none()
            || classification.is_none()
            || promotion_candidate.is_none()
            || cell.get("result").and_then(JsonValue::as_str).is_none()
        {
            return Err("summary JSON has an incomplete repeated-cell result".into());
        }
        if terminal_passes > total || clean_passes > terminal_passes || retried > total {
            return Err("summary JSON has impossible repeated-cell counts".into());
        }
        let counts = RepeatedOutcomeCounts {
            expected_repetitions: expected.unwrap() as usize,
            observed_repetitions: observed.unwrap() as usize,
            qualifying_passes: qualifying.unwrap() as usize,
            clean_passes: clean_passes.unwrap() as usize,
            terminal_passes: terminal_passes.unwrap() as usize,
            product_failures: product_failures.unwrap() as usize,
            infrastructure_failures: infrastructure_failures.unwrap() as usize,
            prerequisite_failures: prerequisite_failures.unwrap() as usize,
            no_results: no_results.unwrap() as usize,
            mixed_repetitions: mixed.unwrap() as usize,
            missing_repetitions: missing.unwrap() as usize,
            unknown_history_repetitions: unknown_history.unwrap() as usize,
            retried_repetitions: retried.unwrap() as usize,
        };
        let expected_classification = classify_pressure_sample(counts);
        if total != expected
            || qualifying > clean_passes
            || unknown_history > expected
            || classification != Some(expected_classification.as_str())
            || promotion_candidate
                != Some(
                    expected_classification
                        == PressureSampleClassification::PromotionCandidate,
                )
        {
            return Err("summary JSON has inconsistent repeated-cell classification".into());
        }
    }
    Ok(())
}

fn summary_heading(metadata: &RunMetadata) -> &'static str {
    let population = population_label(metadata.green, metadata.probe_disabled);
    if metadata.repetitions.is_some() {
        match population {
            "green" => "# Repeated green-cell results",
            "disabled" => "# Repeated disabled-cell results",
            _ => "# Repeated red-cell results",
        }
    } else if metadata.probe_disabled {
        "# Disabled-cell pressure-test results"
    } else {
        "# Red-cell pressure-test results"
    }
}

fn repeated_summary_line(
    metadata: &RunMetadata,
    terminal_passes: usize,
    clean_passes: usize,
    infrastructure_errors: usize,
    retried: usize,
    total: usize,
) -> String {
    let result = top_level_repeated_result_description(
        metadata,
        terminal_passes,
        clean_passes,
        infrastructure_errors,
        retried,
        total,
    );
    if metadata.is_exact() {
        if result == "incomplete" {
            format!(
                "Repeated result: {terminal_passes}/{total} terminally passed; {clean_passes}/{total} passed cleanly; incomplete because {infrastructure_errors} check(s) have no trustworthy result."
            )
        } else {
            format!(
                "Repeated result: {terminal_passes}/{total} terminally passed; {clean_passes}/{total} passed cleanly; {result}."
            )
        }
    } else {
        let population = population_label(metadata.green, metadata.probe_disabled);
        format!(
            "Repeated {population}-cell batch: {terminal_passes}/{total} terminally passed; {clean_passes}/{total} passed cleanly; {result}."
        )
    }
}

fn result_artifact_dir(results: &Path, row: &CellResult) -> Result<PathBuf, String> {
    let path = PathBuf::from(&row.artifact_dir);
    let retained_root = results.join("runs").join(&row.run_id);
    if row.artifact_dir.is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || !path.starts_with(&retained_root)
    {
        return Err(format!(
            "result attempt {} carries artifact directory {} outside {}",
            row.attempt,
            path.display(),
            retained_root.display()
        ));
    }
    Ok(path)
}

fn verification_report_path(artifact_dir: &Path) -> PathBuf {
    artifact_dir.join("verify-1.json")
}

fn retained_verification_logs(
    cell: &CellId,
    artifact_dir: &Path,
) -> Result<Vec<String>, String> {
    if cell.mode != "verify" {
        return Ok(Vec::new());
    }
    let directory = verification_report_path(artifact_dir)
        .parent()
        .expect("verification report has a parent")
        .join("verify-logs")
        .join("verify-1");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut run1 = None;
    let mut run2 = None;
    for entry in fs::read_dir(&directory).map_err(|e| {
        format!(
            "cannot read retained verify logs {}: {e}",
            directory.display()
        )
    })? {
        let entry = entry.map_err(|e| format!("cannot read retained verify-log entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let slot = if name.starts_with("run1_log_") {
            Some(&mut run1)
        } else if name.starts_with("run2_log_") {
            Some(&mut run2)
        } else {
            None
        };
        let Some(slot) = slot else {
            continue;
        };
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot inspect {}: {e}", entry.path().display()))?;
        let metadata = entry
            .metadata()
            .map_err(|e| format!("cannot inspect {}: {e}", entry.path().display()))?;
        if !file_type.is_file() || metadata.len() == 0 {
            return Err(format!(
                "retained verify-log capture {} is not a nonempty regular file",
                entry.path().display()
            ));
        }
        if slot
            .replace(entry.path().to_string_lossy().into_owned())
            .is_some()
        {
            return Err(format!(
                "retained verify-log directory {} contains duplicate {} captures",
                directory.display(),
                if name.starts_with("run1_log_") {
                    "run1"
                } else {
                    "run2"
                }
            ));
        }
    }
    if run1.is_some() != run2.is_some() {
        return Err(format!(
            "retained verify-log directory {} must contain exactly one nonempty run1 capture and one nonempty run2 capture",
            directory.display()
        ));
    }
    Ok(run1.into_iter().chain(run2).collect())
}

fn normalized_ptrace_golden(
    cell: &CellId,
    artifact_dir: &Path,
) -> Result<Option<String>, String> {
    if cell.mode != "verify" || cell.backend != "ptrace" {
        return Ok(None);
    }
    let directory = verification_report_path(artifact_dir)
        .parent()
        .expect("verification report has a parent")
        .join("verify-logs")
        .join("verify-1");
    let status_path = directory.join("normalized-ptrace-golden.status");
    let path = directory.join("normalized-ptrace-golden.log");
    if !status_path.exists() && !path.exists() {
        return Ok(None);
    }
    if !status_path.is_file() {
        return Err(format!(
            "ptrace golden-log output {} exists without its numeric status {}",
            path.display(),
            status_path.display()
        ));
    }
    let status_text = fs::read_to_string(&status_path)
        .map_err(|e| format!("cannot read {}: {e}", status_path.display()))?;
    let status = status_text.trim().parse::<i32>().map_err(|_| {
        format!(
            "{} contains nonnumeric log-diff exit `{}`",
            status_path.display(),
            status_text.trim()
        )
    })?;
    if status != 0 {
        return Err(format!(
            "ptrace golden-log normalization failed with exit {status}; see {}",
            directory.display()
        ));
    }
    if !path.is_file()
        || path
            .metadata()
            .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?
            .len()
            == 0
    {
        return Err(format!(
            "ptrace golden-log normalization reported success without a nonempty {}",
            path.display()
        ));
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn read_verification_report(
    cell: &CellId,
    artifact_dir: &Path,
) -> Result<Option<JsonValue>, String> {
    if !matches!(cell.mode.as_str(), "verify" | "replay") {
        return Ok(None);
    }
    let path = verification_report_path(artifact_dir);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read verification report {}: {e}", path.display()))?;
    let report: JsonValue = serde_json::from_str(&text)
        .map_err(|e| format!("invalid verification report {}: {e}", path.display()))?;
    let canonical = VerificationReport::from_current_json_value(report.clone())
        .map_err(|e| format!("incomplete canonical verification report {}: {e}", path.display()))?;
    match (canonical.verdict, canonical.verified) {
        (Verdict::Matched, true)
        | (Verdict::Diverged | Verdict::NoResult | Verdict::InfrastructureError, false) => {}
        (verdict, verified) => {
            return Err(format!(
                "inconsistent verification report {}: verdict={verdict} verified={verified}",
                path.display()
            ));
        }
    }
    if !matches!(
        canonical.verdict,
        Verdict::NoResult | Verdict::InfrastructureError
    ) && canonical.comparison.is_none()
    {
        return Err(format!(
            "terminal verification report {} has no comparison object",
            path.display()
        ));
    }
    if canonical.bitwise_parity && canonical.verdict != Verdict::Matched {
        return Err(format!(
            "verification report {} claims bitwise parity without a match",
            path.display()
        ));
    }
    if canonical.verdict != Verdict::InfrastructureError || canonical.comparison.is_some() {
        canonical.require_canonical_comparison().map_err(|error| {
            format!(
                "verification report {} cannot support a product verdict: {error}",
                path.display()
            )
        })?;
    }
    if canonical.verdict == Verdict::Matched {
        canonical.require_canonical_match().map_err(|error| {
            format!(
                "verification report {} cannot support a green result: {error}",
                path.display()
            )
        })?;
    }
    if canonical.verdict == Verdict::InfrastructureError
        && canonical.infrastructure_error.is_none()
    {
        return Err(format!(
            "verification report {} names infrastructure_error without its cause",
            path.display()
        ));
    }
    if !matches!(
        canonical.verdict,
        Verdict::Diverged | Verdict::InfrastructureError
    )
        && (canonical.first_divergent_scheduler_turn.is_some()
            || canonical.first_divergent_virtual_nanoseconds.is_some()
            || canonical.first_divergent_record.is_some()
            || canonical.first_divergent_syscall.is_some()
            || canonical.first_divergent_left_message.is_some()
            || canonical.first_divergent_right_message.is_some())
    {
        return Err(format!(
            "verification report {} records divergence evidence without a divergent verdict",
            path.display()
        ));
    }
    Ok(Some(report))
}

fn summarize(
    root: &Path,
    results: &Path,
    allow_dirty_exact_cell: bool,
    typed_runner_evidence: Option<&BTreeMap<String, RunnerEvidence>>,
    fresh: bool,
) -> Result<(), String> {
    let metadata_path = results.join("run.json");
    let metadata: RunMetadata = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .map_err(|e| format!("cannot read {}: {e}", metadata_path.display()))?,
    )
    .map_err(|e| format!("invalid {}: {e}", metadata_path.display()))?;
    if metadata.schema != RUN_SCHEMA {
        return Err(format!("unsupported run schema {}", metadata.schema));
    }
    let current_timeouts = current_result_policy(&metadata, fresh)?;
    let current = git_output(root, &["rev-parse", "HEAD"])?;
    if current != metadata.hermit_sha {
        return Err(format!(
            "run belongs to {}, but checkout HEAD is {}",
            metadata.hermit_sha, current
        ));
    }
    let expected = validate_run_contract(root, results, &metadata, allow_dirty_exact_cell)?;
    let loaded_runner_evidence = if typed_runner_evidence.is_some() {
        None
    } else if let Some(evidence) = load_retained_runner_evidence(results)? {
        Some(evidence)
    } else {
        Some(load_runner_evidence(results, &metadata.hermit_sha)?)
    };
    let runner_evidence = typed_runner_evidence.unwrap_or_else(|| {
        loaded_runner_evidence
            .as_ref()
            .expect("standalone summary loaded runner evidence")
    });
    let expected_runner_tags: BTreeSet<String> = metadata
        .cells
        .iter()
        .flat_map(|cell| {
            repetition_numbers(metadata.repetitions)
                .map(move |repetition| format!("cell.{}", cell_run_slug(cell, repetition)))
        })
        .collect();
    let actual_runner_tags: BTreeSet<String> = runner_evidence.keys().cloned().collect();
    if actual_runner_tags != expected_runner_tags {
        let missing = expected_runner_tags
            .difference(&actual_runner_tags)
            .next()
            .cloned()
            .unwrap_or_else(|| "none".into());
        let foreign = actual_runner_tags
            .difference(&expected_runner_tags)
            .next()
            .cloned()
            .unwrap_or_else(|| "none".into());
        return Err(format!(
            "typed scheduler cell identities do not match the selected runs: expected={} actual={} first_missing={} first_foreign={}",
            expected_runner_tags.len(),
            actual_runner_tags.len(),
            missing,
            foreign
        ));
    }

    let mut by_backend: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut sample_counts = BTreeMap::<CellId, RepeatedOutcomeCounts>::new();
    let mut repeated_terminal_passes = BTreeMap::<CellId, usize>::new();
    let mut repeated_clean_passes = BTreeMap::<CellId, usize>::new();
    let mut repeated_infrastructure_errors = BTreeMap::<CellId, usize>::new();
    let mut repeated_totals = BTreeMap::<CellId, usize>::new();
    let mut retried_by_cell = BTreeMap::<CellId, usize>::new();
    let mut retried_repetitions = 0usize;
    let mut attempted = 0usize;
    let mut passing = Vec::new();
    let mut rows = Vec::new();
    for cell in &metadata.cells {
        for repetition in repetition_numbers(metadata.repetitions) {
            let slug = cell_run_slug(cell, repetition);
            let evidence_run_id =
                cell_evidence_run_id(cell, repetition, metadata.run_id_prefix.as_deref());
            let cell_dir = results.join("cells").join(&slug);
            let step_tag = format!("cell.{slug}");
            let runner = runner_evidence.get(&step_tag).copied().unwrap_or_default();
            let runner_output_log = runner_output_log(&step_tag, runner.output_log_available);
            let mut evidence_errors = Vec::new();
            let status_file = cell_dir.join("harness-status");
            let harness_status = if status_file.is_file() {
                match fs::read_to_string(&status_file) {
                    Ok(text) => {
                        let text = text.trim();
                        match text.parse::<i32>() {
                            Ok(status) => Some(status),
                            Err(_) => {
                                evidence_errors.push(format!(
                                    "{} contains nonnumeric harness exit `{text}`",
                                    status_file.display()
                                ));
                                None
                            }
                        }
                    }
                    Err(error) => {
                        evidence_errors.push(format!(
                            "cannot read harness exit {}: {error}",
                            status_file.display()
                        ));
                        None
                    }
                }
            } else {
                evidence_errors.push(format!(
                    "selected cell node wrote no attempt marker at {}",
                    status_file.display()
                ));
                None
            };
            let proven_oom = is_proven_oom_attempt(runner, harness_status);
            let proven_timeout = is_proven_timeout_attempt(runner, harness_status);
            let result_file = cell_dir.join("results.jsonl");
            // Sample classification explains absent product evidence separately.
            // The existing row diagnostics, result and counters below remain intact.
            let mut sample_evidence_errors = Vec::new();
            let initial_evidence_valid = evidence_errors.is_empty();
            let result_file_size = match fs::metadata(&result_file) {
                Ok(metadata) if metadata.is_file() => Some(metadata.len()),
                Ok(_) => {
                    sample_evidence_errors.push("result history is not a regular file".into());
                    None
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    sample_evidence_errors.push(format!("cannot inspect result history: {error}"));
                    None
                }
            };
            let prepared_empty_result_file = result_file_size == Some(0);
            let rejected_result_history = result_file_size.is_some_and(|size| size > 0);
            let retained_prerequisite = if prepared_empty_result_file {
                match retained_host_inapplicable(&cell_dir, cell) {
                    Ok(true) if harness_status.is_some_and(|status| status != 0)
                        && runner_observed_terminal_attempt(runner, harness_status) => true,
                    Ok(true) => {
                        sample_evidence_errors.push(
                            "host-inapplicable summary has no matching completed nonzero scheduler node".into()
                        );
                        false
                    }
                    Ok(false) => false,
                    Err(error) => {
                        sample_evidence_errors.push(error);
                        false
                    }
                }
            } else {
                false
            };
            let mut observations = Vec::new();
            let mut result_rows_for_history = Vec::new();
            let (
                outcome,
                row_valid,
                reason,
                error_kind,
                recorded_result,
                failure_class,
                attempt,
                invocation,
                artifact_dir,
            ) = if result_file.is_file() {
                let result_rows = if current_timeouts {
                    read_current_result_rows(&result_file)
                } else {
                    read_result_rows(&result_file)
                };
                match result_rows {
                    Ok(result_rows) => {
                        observations = result_rows
                            .iter()
                            .map(|row| {
                                json!({
                                    "attempt": row.attempt,
                                    "outcome": row.outcome,
                                    "result": row.result,
                                    "failure_class": row.failure_class,
                                    "reason": row.reason,
                                    "error_kind": row.error_kind,
                                    "duration_ms": row.duration_ms,
                                    "timeout_seconds": row.timeout_seconds,
                                    "artifact_dir": row.artifact_dir,
                                    "first_divergent_record": row.first_divergent_record,
                                    "first_divergent_syscall": row.first_divergent_syscall,
                                    "first_divergent_scheduler_turn": row.first_divergent_scheduler_turn,
                                    "first_divergent_virtual_nanoseconds": row.first_divergent_virtual_nanoseconds,
                                    "first_divergent_left_message": row.first_divergent_left_message,
                                    "first_divergent_right_message": row.first_divergent_right_message,
                                })
                            })
                            .collect();
                        result_rows_for_history = result_rows.clone();
                        let expected_required = expected.get(cell).copied().unwrap_or(false);
                        let identities_match = result_rows.iter().all(|row| {
                            result_row_identity_and_invocation_match(
                                row,
                                &evidence_run_id,
                                &metadata,
                                cell,
                                expected_required,
                            )
                        });
                        let artifact_dirs = result_rows
                            .iter()
                            .map(|row| result_artifact_dir(results, row))
                            .collect::<Result<Vec<_>, _>>();
                        let row = cell_result_after_retries(&result_rows)?;
                        let row_matches = result_row_matches_cell(
                            row,
                            &evidence_run_id,
                            &metadata,
                            cell,
                            expected_required,
                            harness_status,
                        );
                        let runner_completed =
                            runner_observed_terminal_attempt(runner, harness_status);
                        match artifact_dirs {
                            Ok(_)
                                if identities_match
                                    && row_matches
                                    && (proven_oom || runner_completed) =>
                            {
                                match result_row_invocation(row) {
                                    Ok(invocation) => {
                                        (
                                            row.outcome.clone(),
                                            true,
                                            row.reason.clone(),
                                            row.error_kind.clone(),
                                            row.result,
                                            row.failure_class,
                                            row.attempt,
                                            Some(invocation),
                                            Some(result_artifact_dir(results, row)?),
                                        )
                                    }
                                    Err(error) => {
                                        evidence_errors.push(format!(
                                            "{} does not carry complete literal attempt invocations: {error}",
                                            result_file.display()
                                        ));
                                        (
                                            "NO_RESULT".to_string(),
                                            false,
                                            None,
                                            None,
                                            None,
                                            None,
                                            1,
                                            None,
                                            None,
                                        )
                                    }
                                }
                            }
                            Ok(_) => {
                                evidence_errors.push(format!(
                                    "{} does not match every selected-cell observation, the terminal harness exit, or retained runner result",
                                    result_file.display()
                                ));
                                (
                                    "NO_RESULT".to_string(),
                                    false,
                                    None,
                                    None,
                                    None,
                                    None,
                                    1,
                                    None,
                                    None,
                                )
                            }
                            Err(error) => {
                                evidence_errors.push(error);
                                (
                                    "NO_RESULT".to_string(),
                                    false,
                                    None,
                                    None,
                                    None,
                                    None,
                                    1,
                                    None,
                                    None,
                                )
                            }
                        }
                    }
                    Err(error) => {
                        evidence_errors.push(error);
                        (
                            "NO_RESULT".to_string(),
                            false,
                            None,
                            None,
                            None,
                            None,
                            1,
                            None,
                            None,
                        )
                    }
                }
            } else if !proven_oom && !proven_timeout {
                evidence_errors.push(format!("missing result row {}", result_file.display()));
                (
                    "NO_RESULT".to_string(),
                    false,
                    None,
                    None,
                    None,
                    None,
                    1,
                    None,
                    None,
                )
            } else {
                (
                    "NO_RESULT".to_string(),
                    false,
                    None,
                    None,
                    None,
                    None,
                    1,
                    None,
                    None,
                )
            };
            let mut typed_no_comparison_refusal = false;
            let verification = match artifact_dir.as_deref() {
                Some(artifact_dir) => match read_verification_report(cell, artifact_dir) {
                    Ok(Some(report)) => Some(report),
                    Ok(None)
                        if matches!(cell.mode.as_str(), "verify" | "replay")
                            && !proven_oom
                            && !proven_timeout =>
                    {
                        evidence_errors.push(format!(
                            "missing verification report {}",
                            verification_report_path(artifact_dir).display()
                        ));
                        None
                    }
                    Ok(None) => None,
                    Err(error) => {
                        typed_no_comparison_refusal = retained_typed_no_comparison(cell, artifact_dir, &result_rows_for_history);
                        evidence_errors.push(error);
                        None
                    }
                },
                None => None,
            };
            let verification_verdict = verification
                .as_ref()
                .and_then(|report| report.get("verdict"))
                .and_then(JsonValue::as_str);
            let verification_logs = match artifact_dir.as_deref() {
                Some(artifact_dir) => match retained_verification_logs(cell, artifact_dir) {
                    Ok(logs) => logs,
                    Err(error) => {
                        evidence_errors.push(error);
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };
            let normalized_ptrace_golden = match artifact_dir.as_deref() {
                Some(artifact_dir) => match normalized_ptrace_golden(cell, artifact_dir) {
                    Ok(path) => path,
                    Err(error) => {
                        evidence_errors.push(error);
                        None
                    }
                },
                None => None,
            };
            if cell.mode == "verify"
                && matches!(verification_verdict, Some("matched" | "diverged"))
                && verification_logs.len() != 2
            {
                evidence_errors.push(
                "terminal verify result must retain exactly one nonempty run1 log and one nonempty run2 log"
                    .into(),
            );
            }
            if cell.mode == "verify"
                && cell.backend == "ptrace"
                && matches!(verification_verdict, Some("matched" | "diverged"))
                && normalized_ptrace_golden.is_none()
                && !evidence_errors
                    .iter()
                    .any(|error| error.contains("golden-log normalization"))
            {
                evidence_errors
                    .push("terminal ptrace verify result has no normalized golden INFO log".into());
            }
            if invocation.is_none() {
                evidence_errors.push("selected result has no complete recorded invocation".into());
            }
            let derived_result = classify_result(
                runner,
                harness_status,
                &outcome,
                row_valid,
                reason.as_deref(),
                &cell.mode,
                verification_verdict,
                verification_logs.len() == 2,
                evidence_errors.is_empty(),
            );
            // Current rows take their functional result from the framework.
            // The older pressure classifier remains only to read retained
            // pre-field rows and to refuse disagreement; it is no longer the
            // authority that reconstructs a current result after execution.
            let mut result = derived_result;
            if row_valid && evidence_errors.is_empty() {
                match reconcile_recorded_result(recorded_result, failure_class, derived_result) {
                    Ok(recorded) => result = recorded,
                    Err(error) => evidence_errors.push(error),
                }
            }
            if !evidence_errors.is_empty() {
                result = "infrastructure-error";
            }
            let retained_attempts = retained_attempt_count(
                &result_rows_for_history,
                &evidence_run_id,
                &metadata,
                cell,
                expected.get(cell).copied().unwrap_or(false),
                runner,
                harness_status,
            )?;
            attempted = attempted
                .checked_add(retained_attempts)
                .ok_or("pressure attempt count overflowed usize")?;
            *by_backend
                .entry(cell.backend.clone())
                .or_default()
                .entry(result.to_string())
                .or_default() += 1;
            if metadata.repetitions.is_some() {
                *repeated_totals.entry(cell.clone()).or_default() += 1;
                if result == "pass" {
                    *repeated_terminal_passes.entry(cell.clone()).or_default() += 1;
                }
                if repetition_passed_cleanly(result, &result_rows_for_history) {
                    *repeated_clean_passes.entry(cell.clone()).or_default() += 1;
                }
                if matches!(result, "infrastructure-error" | "sandbox-denied") {
                    *repeated_infrastructure_errors
                        .entry(cell.clone())
                        .or_default() += 1;
                }
                let counts = sample_counts.entry(cell.clone()).or_default();
                counts.expected_repetitions += 1;
                counts.clean_passes += usize::from(repetition_passed_cleanly(result, &result_rows_for_history));
                counts.retried_repetitions += usize::from(retained_attempts > 1);
                let inner_history = if result_rows_for_history.is_empty() {
                    None
                } else {
                    Some(inner_pressure_history(&result_rows_for_history))
                };
                // A typed NoResult stamp legitimately has no comparison. Only
                // that one verified reader refusal may be explained here; missing
                // captures, golden output or other artifact errors stay incomplete.
                let sample_artifacts_valid = evidence_errors.is_empty()
                    || (typed_no_comparison_refusal && evidence_errors.len() == 1);
                counts.unknown_history_repetitions += usize::from(
                    inner_history.as_ref().is_some_and(|history| history.is_err())
                        || (row_valid && !sample_artifacts_valid)
                );
                if let Some(Err(error)) = &inner_history {
                    sample_evidence_errors.push(error.clone());
                }
                if result == "pass" {
                    counts.observed_repetitions += 1;
                    counts.terminal_passes += 1;
                    counts.qualifying_passes += usize::from(
                        evidence_errors.is_empty()
                            && sample_evidence_errors.is_empty()
                            && repetition_qualifies_for_promotion(result, &result_rows_for_history)
                    );
                } else {
                    let classification = if !sample_evidence_errors.is_empty() && !row_valid {
                        RepetitionClassification::Missing
                    } else {
                        let outer = classify_nonpassing_repetition(
                            result, &result_rows_for_history, row_valid,
                            if prepared_empty_result_file {
                                initial_evidence_valid
                            } else {
                                sample_artifacts_valid
                            },
                            rejected_result_history, proven_timeout, proven_oom,
                            retained_prerequisite,
                        );
                        match &inner_history {
                            Some(Ok(inner)) => fold_pressure_history(outer, inner),
                            _ => outer,
                        }
                    };
                    match classification {
                        RepetitionClassification::ProductFailure => counts.product_failures += 1,
                        RepetitionClassification::InfrastructureFailure => counts.infrastructure_failures += 1,
                        RepetitionClassification::PrerequisiteFailure => counts.prerequisite_failures += 1,
                        RepetitionClassification::NoResult => counts.no_results += 1,
                        RepetitionClassification::Mixed => counts.mixed_repetitions += 1,
                        RepetitionClassification::Missing => counts.missing_repetitions += 1,
                    }
                    counts.observed_repetitions += usize::from(classification != RepetitionClassification::Missing);
                }
                if retained_attempts > 1 {
                    retried_repetitions = retried_repetitions
                        .checked_add(1)
                        .ok_or("pressure retried-repetition count overflowed usize")?;
                    *retried_by_cell.entry(cell.clone()).or_default() += 1;
                }
            }
            if result == "pass" && metadata.repetitions.is_none() {
                passing.push(display_id(cell));
            }
            // ⚠️ AN EARLIER ATTEMPT THAT DIVERGED IS STILL AN OBSERVATION.
            // The row above reports the framework-selected cell result: a passing
            // retry is green, while a product failure stays red if every retry
            // fails. Any other attempt that located a divergence is emitted as its
            // own row carrying the same repetition and its own attempt ordinal, so
            // the observation is not dropped when a retry happens to pass.
            //
            // ⚠️ THIS CANNOT MOVE A STATUS. These rows only add observations, and
            // observations feed `measurement`; `status` is owned by a different
            // writer and the scorecard enforces that boundary.
            if row_valid {
                if let Some(terminal) = result_rows_for_history.last() {
                    for earlier_row in earlier_attempts_that_located(
                        &result_rows_for_history,
                        terminal.attempt,
                    ) {
                        if earlier_row.attempt == attempt {
                            continue;
                        }
                        if let Some(recorded_result) = earlier_row.result {
                            if earlier_row.failure_class != recorded_result.failure_class() {
                                return Err(format!(
                                    "earlier framework attempt {} result {} carries failure_class {:?}, expected {:?}",
                                    earlier_row.attempt,
                                    recorded_result.as_str(),
                                    earlier_row.failure_class,
                                    recorded_result.failure_class()
                                ));
                            }
                        }
                        let earlier_invocation = result_row_invocation(earlier_row)?;
                        let earlier_artifact_dir = result_artifact_dir(results, earlier_row)?;
                        let earlier_verification = read_verification_report(
                            cell,
                            &earlier_artifact_dir,
                        )?
                        .ok_or_else(|| {
                            format!(
                                "earlier attempt {} located a divergence but has no verification report at {}",
                                earlier_row.attempt,
                                verification_report_path(&earlier_artifact_dir).display()
                            )
                        })?;
                        let earlier_verification_logs =
                            retained_verification_logs(cell, &earlier_artifact_dir)?;
                        let earlier_normalized_ptrace_golden =
                            crate::normalized_ptrace_golden(cell, &earlier_artifact_dir)?;
                        let expected_result = match cell.mode.as_str() {
                            "verify" => ObservedResult::DeterminismFailure,
                            "replay" => ObservedResult::ReplayFailure,
                            other => {
                                return Err(format!(
                                    "earlier attempt {} located a divergence in unsupported mode {other}",
                                    earlier_row.attempt
                                ));
                            }
                        };
                        let earlier_result = match earlier_row.result {
                            Some(recorded) if recorded != expected_result => {
                                return Err(format!(
                                    "earlier framework attempt {} records result {}, but its retained report is a {} divergence",
                                    earlier_row.attempt,
                                    recorded.as_str(),
                                    cell.mode
                                ));
                            }
                            Some(recorded) => recorded,
                            None => expected_result,
                        };
                        rows.push(json!({
                            "cell": cell,
                            "repetition": repetition,
                            "attempt": earlier_row.attempt,
                            "harness_exit": harness_status,
                            "outcome": earlier_row.outcome,
                            "failure_class": earlier_row.failure_class,
                            "reason": earlier_row.reason,
                            "error_kind": earlier_row.error_kind,
                            "invocation": earlier_invocation,
                            "result_row_valid": true,
                            "result": earlier_result.as_str(),
                            "verification": earlier_verification,
                            "verification_logs": earlier_verification_logs,
                            "normalized_ptrace_golden": earlier_normalized_ptrace_golden,
                            "evidence_errors": Vec::<String>::new(),
                            "runner_seen": runner.seen,
                            "runner_ok": runner.ok,
                            "runner_timed_out": runner.timed_out,
                            "runner_oom": runner.oom,
                            "runner_output_observed": !matches!(
                                runner.environmental_block_observation,
                                EnvBlockObservation::NothingObserved
                            ),
                            "runner_environmental_block_class": runner.environmental_block_observation.class(),
                            "runner_output_log": runner_output_log,
                            "oom_proven_by_runner_and_attempt_marker": false,
                            "timeout_proven_by_runner_and_attempt_marker": false,
                        }));
                    }
                }
            }
            rows.push(json!({
                "cell": cell,
                "repetition": repetition,
                "attempt": attempt,
                "harness_exit": harness_status,
                "outcome": outcome,
                "failure_class": failure_class,
                "reason": reason,
                "error_kind": error_kind,
                "observations": observations,
                "sample_evidence_errors": sample_evidence_errors,
                "invocation": invocation,
                "result_row_valid": row_valid,
                "result": result,
                "verification": verification,
                "verification_logs": verification_logs,
                "normalized_ptrace_golden": normalized_ptrace_golden,
                "evidence_errors": evidence_errors,
                "runner_seen": runner.seen,
                "runner_ok": runner.ok,
                "runner_timed_out": runner.timed_out,
                "runner_oom": runner.oom,
                "runner_output_observed": !matches!(
                    runner.environmental_block_observation,
                    EnvBlockObservation::NothingObserved
                ),
                "runner_environmental_block_class": runner.environmental_block_observation.class(),
                "runner_output_log": runner_output_log,
                "oom_proven_by_runner_and_attempt_marker": proven_oom,
                "timeout_proven_by_runner_and_attempt_marker": proven_timeout,
            }));
        }
    }
    println!("{}", summary_heading(&metadata));
    println!();
    println!(
        "Final-result denominator: one framework-selected result per selected cell repetition; `attempted` counts every attributable harness attempt."
    );
    println!();
    println!(
        "Metric: current pre-basic-sanity manifest contract. Verify uses the legacy stripped comparison unless that cell's verification report says bitwise_parity=true; this is not the Milestone 2 strict-default metric."
    );
    println!();
    if metadata.source_tree_dirty {
        println!(
            "**Exploratory result from a dirty working tree: this cannot promote the scorecard.**"
        );
        println!();
    }
    println!(
        "| Backend | Pass | Determinism failure | Replay failure | Crash/error | Timeout | OOM | Sandbox denied | Infrastructure error | Total |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    let mut totals = [0usize; 9];
    for backend in ["ptrace", "dbt", "kvm", "sabre", "liteinst", "native"] {
        let counts = by_backend.get(backend).cloned().unwrap_or_default();
        let pass = counts.get("pass").copied().unwrap_or(0);
        let determinism = counts.get("determinism-failure").copied().unwrap_or(0);
        let replay = counts.get("replay-failure").copied().unwrap_or(0);
        let crash_error = counts.get("crash-error").copied().unwrap_or(0);
        let timeout = counts.get("timeout").copied().unwrap_or(0);
        let oom = counts.get("oom").copied().unwrap_or(0);
        let sandbox_denied = counts.get("sandbox-denied").copied().unwrap_or(0);
        let infrastructure = counts.get("infrastructure-error").copied().unwrap_or(0);
        let total = pass
            + determinism
            + replay
            + crash_error
            + timeout
            + oom
            + sandbox_denied
            + infrastructure;
        totals[0] += pass;
        totals[1] += determinism;
        totals[2] += replay;
        totals[3] += crash_error;
        totals[4] += timeout;
        totals[5] += oom;
        totals[6] += sandbox_denied;
        totals[7] += infrastructure;
        totals[8] += total;
        println!(
            "| `{backend}` | {pass} | {determinism} | {replay} | {crash_error} | {timeout} | {oom} | {sandbox_denied} | {infrastructure} | {total} |"
        );
    }
    println!(
        "| **Total** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** |",
        totals[0], totals[1], totals[2], totals[3], totals[4], totals[5], totals[6], totals[7], totals[8]
    );
    println!();
    println!(
        "Crash/error combines remaining nonzero harness exits. It includes signal-caused crashes when the shell reports a nonzero status; this runner does not yet distinguish the signal."
    );
    println!();
    if metadata.repetitions.is_none()
        && metadata.cells.len() == 1
        && matches!(metadata.cells[0].mode.as_str(), "verify" | "replay")
    {
        let verification = &rows[0]["verification"];
        if verification.is_object() {
            println!(
                "Exact-cell verification: verdict={} bitwise_parity={} comparison={}",
                verification["verdict"], verification["bitwise_parity"], verification["comparison"]
            );
        } else {
            println!(
                "Exact-cell verification: no trustworthy terminal report; see evidence_errors in summary.json."
            );
        }
        println!();
    }
    let mut repeated_cells = Vec::new();
    let repeated_terminal_pass_count: usize = repeated_terminal_passes.values().sum();
    let repeated_clean_pass_count: usize = repeated_clean_passes.values().sum();
    let repeated_total_count: usize = repeated_totals.values().sum();
    let repeated_result = if metadata.repetitions.is_some() && metadata.is_exact() {
        let cell = &metadata.cells[0];
        let terminal_passes = repeated_terminal_passes.get(cell).copied().unwrap_or(0);
        let clean_passes = repeated_clean_passes.get(cell).copied().unwrap_or(0);
        let infrastructure_errors = repeated_infrastructure_errors
            .get(cell)
            .copied()
            .unwrap_or(0);
        let retried = retried_by_cell.get(cell).copied().unwrap_or(0);
        let total = repeated_totals.get(cell).copied().unwrap_or(0);
        let result = top_level_repeated_result_description(
            &metadata,
            terminal_passes,
            clean_passes,
            infrastructure_errors,
            retried,
            total,
        );
        println!(
            "{}",
            repeated_summary_line(
                &metadata,
                terminal_passes,
                clean_passes,
                infrastructure_errors,
                retried,
                total,
            )
        );
        let counts = sample_counts.get(cell).copied().unwrap_or_default();
        println!("Sample classification for `{}`: {}; {}/{} qualifying first attempts.",
            display_id(cell), classify_pressure_sample(counts).as_str(),
            counts.qualifying_passes, counts.expected_repetitions);
        repeated_cells.push(repeated_cell_summary(cell, counts, result));
        Some(result)
    } else if metadata.repetitions.is_some() {
        println!("| Cell | Terminal passes | Clean passes | Result |");
        println!("| --- | ---: | ---: | --- |");
        for cell in &metadata.cells {
            let terminal_passes = repeated_terminal_passes.get(cell).copied().unwrap_or(0);
            let clean_passes = repeated_clean_passes.get(cell).copied().unwrap_or(0);
            let infrastructure_errors = repeated_infrastructure_errors
                .get(cell)
                .copied()
                .unwrap_or(0);
            let retried = retried_by_cell.get(cell).copied().unwrap_or(0);
            let total = repeated_totals.get(cell).copied().unwrap_or(0);
            let result = repeated_result_description(
                terminal_passes,
                clean_passes,
                infrastructure_errors,
                retried,
                total,
            );
            println!(
                "| `{}` | {terminal_passes}/{total} | {clean_passes}/{total} | {result} |",
                display_id(cell)
            );
            let counts = sample_counts.get(cell).copied().unwrap_or_default();
        println!("Sample classification for `{}`: {}; {}/{} qualifying first attempts.",
            display_id(cell), classify_pressure_sample(counts).as_str(),
            counts.qualifying_passes, counts.expected_repetitions);
        repeated_cells.push(repeated_cell_summary(cell, counts, result));
        }
        println!();
        let infrastructure_errors: usize = repeated_infrastructure_errors.values().sum();
        let result = top_level_repeated_result_description(
            &metadata,
            repeated_terminal_pass_count,
            repeated_clean_pass_count,
            infrastructure_errors,
            retried_repetitions,
            repeated_total_count,
        );
        println!(
            "{}",
            repeated_summary_line(
                &metadata,
                repeated_terminal_pass_count,
                repeated_clean_pass_count,
                infrastructure_errors,
                retried_repetitions,
                repeated_total_count,
            )
        );
        Some(result)
    } else {
        let population = population_label(metadata.green, metadata.probe_disabled);
        println!(
            "{} {population} cell(s) passed once; they are candidates for repeated confirmation, not automatic promotion.",
            passing.len(),
        );
        for id in passing.iter().take(20) {
            println!("  PASS {id}");
        }
        None
    };

    let summary = json!({
        "schema": SUMMARY_SCHEMA,
        "hermit_sha": metadata.hermit_sha,
        "detcore_tree": metadata.detcore_tree,
        "source_tree_dirty": metadata.source_tree_dirty,
        "mode": metadata.mode,
        "test": metadata.test,
        "backend": metadata.backend,
        "cell_timeout_seconds": metadata.cell_timeout_seconds,
        "sample": metadata.sample,
        "seed": metadata.seed,
        "unavailable_cells": metadata.unavailable_cells,
        "repetitions": metadata.repetitions,
        "run_id_prefix": metadata.run_id_prefix,
        "green": metadata.green,
        "probe_disabled": metadata.probe_disabled,
        "jobs": metadata.jobs,
        "eligible_cells": (metadata.eligible_cells != 0).then_some(metadata.eligible_cells),
        "selected_cells": metadata.cells.len(),
        "retried_repetitions": retried_repetitions,
        "repeated_result": repeated_result,
        "repeated_cells": repeated_cells,
        "attempted": attempted,
        "pass_candidates": passing,
        "rows": rows,
    });
    verify_repetition_summary_json(&summary, attempted, retried_repetitions)?;
    let mut text = serde_json::to_string_pretty(&summary)
        .map_err(|e| format!("cannot serialize summary: {e}"))?;
    text.push('\n');
    fs::write(results.join("summary.json"), text)
        .map_err(|e| format!("cannot write summary.json: {e}"))?;
    println!("Summary: {}", results.join("summary.json").display());
    if totals[6] > 0 {
        return Err(format!(
            "{} selected cell run(s) were sandbox-denied before the requested operation completed; retained stdout/stderr names the BPFJailer denial",
            totals[6]
        ));
    }
    if totals[7] > 0 {
        return Err(format!(
            "{} selected cell run(s) produced no trustworthy result; these are harness/infrastructure errors, not compatibility evidence",
            totals[7]
        ));
    }
    let repeated_red = metadata.repetitions.is_some() && !metadata.green;
    if repeated_run_has_unacceptable_product_result(
        metadata.repetitions,
        repeated_red,
        repeated_clean_pass_count,
        retried_repetitions,
        repeated_total_count,
    ) {
        return Err(format!(
            "only {}/{} repeated green-cell checks passed cleanly; {} repetition(s) required a retry, and the retained summary classifies every non-pass",
            repeated_clean_pass_count, repeated_total_count, retried_repetitions
        ));
    }
    Ok(())
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn literal_shell_command(
    cwd: &str,
    env: &BTreeMap<String, String>,
    argv: &[String],
) -> String {
    let mut words = vec![
        "cd".into(),
        recorded_shell_quote(cwd),
        "&&".into(),
        "env".into(),
    ];
    words.extend(
        env.iter()
            .map(|(name, value)| recorded_shell_quote(&format!("{name}={value}"))),
    );
    words.extend(argv.iter().map(|arg| recorded_shell_quote(arg)));
    words.join(" ")
}

fn recorded_shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn display_id(cell: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        cell.lane, cell.category, cell.test, cell.mode, cell.backend
    )
}

fn prerequisite_scheduler_self_test(canonical: &DagConfig, scratch: &Path) -> Result<(), String> {
    let required = required_build_tags(None, true);
    let original: BTreeMap<_, _> = canonical.steps.iter()
        .filter(|step| required.contains(step.tag().as_str()))
        .map(|step| (step.tag(), step.clone())).collect();
    for failed in [None, Some("pre.submodules"), Some("pre.reverie_pin"),
        Some("build.rust_scripts"), Some("gate.manifest")]
    {
        let log = scratch.join(format!("prerequisites-{}", failed.unwrap_or("positive")));
        let mut fixture = canonical.clone();
        fixture.steps = original.values().cloned().collect();
        for step in &mut fixture.steps {
            retain_required_build_dependencies(step, &required)?;
            let tag = step.tag();
            // Delay the planted failure so accidentally unguarded consumers
            // have time to leave a sentinel. The assertions retain the exact
            // canonical dependency chain and do not rely on dispatch ordering.
            step.cmd = format!("{}printf '%s\\n' {} >> {}; exit {}",
                if failed == Some(tag.as_str()) { "sleep 0.1; " } else { "" },
                shell_quote(&tag), shell_quote(&log.to_string_lossy()),
                if failed == Some(tag.as_str()) { 17 } else { 0 });
            step.env.clear();
            step.timeout = 5;
            step.cpu_timeout = 5;
            step.jobs_flag = None;
            step.jobs_env = None;
            step.hint = ResourceHint {
                rss_baseline_bytes: Some(67_108_864),
                hard_mem_max_bytes: Some(67_108_864),
                classification: StepClass::Light,
                ..ResourceHint::default()
            };
        }
        let result = with_execution_root(scratch, || {
            execute_typed_dag(&fixture, 4, None, Instant::now(), 100)
        });
        let mut expected = BTreeSet::new();
        if let Some(failed) = failed {
            let error = result.err().ok_or_else(|| format!("failed prerequisite {failed} was accepted"))?;
            if !error.contains(&format!("pressure setup node {failed} failed:")) {
                return Err(format!("failed prerequisite {failed} lost its diagnostic: {error}"));
            }
            let mut pending = vec![failed.to_string()];
            while let Some(tag) = pending.pop() {
                if expected.insert(tag.clone()) {
                    pending.extend(original[&tag].deps.iter().cloned());
                }
            }
        } else {
            let execution = result?;
            if execution.outcomes.len() != 10 || execution.outcomes.iter().any(|outcome| !outcome.ok) {
                return Err("positive prerequisite fixture did not execute all ten nodes".into());
            }
            expected.extend(original.keys().cloned());
        }
        let text = fs::read_to_string(&log)
            .map_err(|e| format!("cannot read prerequisite sentinels: {e}"))?;
        let actual: BTreeSet<String> = text.lines().map(str::to_string).collect();
        if actual != expected || text.lines().count() != expected.len() {
            return Err(format!("prerequisite failure {failed:?} admitted a consumer or lost an ancestor: expected={expected:?} actual={actual:?}"));
        }
    }
    println!("  prerequisite scheduler: ten-node positive and four failed-preflight controls retain exact execution identities");
    Ok(())
}

fn retained_termination_self_test(scratch: &Path) -> Result<(), String> {
    let results = scratch.join("typed-termination");
    fs::create_dir_all(&results).map_err(|error| error.to_string())?;
    let path = results.join("runner-outcomes.json");
    let presentations = [
        "unrelated presentation",
        "TIMEOUT >1s",
        "CPU-TIMEOUT >1s",
        "OOM-KILLED",
        "",
    ];
    let mut outcomes = Vec::new();
    for bits in 0_u8..8 {
        for (index, presentation) in presentations.iter().enumerate() {
            let mut outcome = StepOutcome::failed(
                format!("cell.flags-{bits}-text-{index}"),
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
                DEFAULT_CPU_TIMEOUT_MULTIPLIER,
                "",
                false,
                None,
                None,
            );
            outcome.reason = (*presentation).into();
            outcomes.push(outcome);
        }
    }
    let execution = ExecutionEvidence {
        outcomes,
        passes: 1,
        scheduler_wall_s: 0.0,
        step_profile_rows: Vec::new(),
    };
    let immediate = retain_execution_evidence(&results, &execution)?;
    let retained_bytes = fs::read(&path).map_err(|error| error.to_string())?;
    let retained: JsonValue =
        serde_json::from_slice(&retained_bytes).map_err(|error| error.to_string())?;
    if retained["schema"] != 2 || retained["scheduler_passes"] != 1 {
        return Err("typed termination writer did not emit the current schema".into());
    }
    let loaded =
        load_retained_runner_evidence(&results)?.ok_or("typed termination file disappeared")?;
    if immediate.len() != 40 || immediate != loaded {
        return Err("typed termination retention lost an identity or changed its facts".into());
    }
    for bits in 0_u8..8 {
        for index in 0..presentations.len() {
            let tag = format!("cell.flags-{bits}-text-{index}");
            let row = loaded
                .get(&tag)
                .ok_or_else(|| format!("typed termination lost {tag}"))?;
            if !row.seen || row.ok || row.oom != (bits & 1 != 0) || row.timed_out != (bits & 6 != 0)
            {
                return Err(format!(
                    "typed termination classified presentation instead of facts for {tag}: {row:?}"
                ));
            }
        }
    }
    if fs::read(&path).map_err(|error| error.to_string())? != retained_bytes {
        return Err("loading current termination evidence changed its bytes".into());
    }

    let mut document = retained.clone();
    document["outcomes"] = json!([retained["outcomes"][0].clone()]);
    let refuse = |label: &str, value: &JsonValue| -> Result<(), String> {
        fs::write(
            &path,
            serde_json::to_vec(value).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if load_retained_runner_evidence(&results).is_ok() {
            return Err(format!(
                "typed termination reader accepted {label}: {value}"
            ));
        }
        Ok(())
    };
    for schema in [
        json!(0),
        json!(3),
        json!(-1),
        json!("2"),
        json!(true),
        JsonValue::Null,
    ] {
        let mut bad = document.clone();
        bad["schema"] = schema;
        refuse("unknown or malformed schema", &bad)?;
    }
    let mut missing_schema = document.clone();
    missing_schema.as_object_mut().unwrap().remove("schema");
    refuse("missing schema", &missing_schema)?;
    for field in ["oomed", "oom_kills", "timed_out", "cpu_timed_out"] {
        let mut missing = document.clone();
        missing["outcomes"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
        refuse(&format!("missing {field}"), &missing)?;
        for value in [JsonValue::Null, json!("false")] {
            let mut wrong = document.clone();
            wrong["outcomes"][0][field] = value;
            refuse(&format!("malformed {field}"), &wrong)?;
        }
    }
    for (oomed, kills) in [(false, -1), (false, 1), (true, 0)] {
        let mut bad = document.clone();
        bad["outcomes"][0]["oomed"] = json!(oomed);
        bad["outcomes"][0]["oom_kills"] = json!(kills);
        refuse("contradictory OOM count", &bad)?;
    }
    for field in ["oomed", "timed_out", "cpu_timed_out"] {
        let mut bad = document.clone();
        bad["outcomes"][0]["ok"] = json!(true);
        bad["outcomes"][0][field] = json!(true);
        if field == "oomed" {
            bad["outcomes"][0]["oom_kills"] = json!(1);
        }
        refuse("successful resource termination", &bad)?;
    }
    for schema in [1, 2] {
        let mut exact = document.clone();
        exact["schema"] = json!(schema);
        if schema == 1 {
            for field in ["oomed", "oom_kills", "timed_out", "cpu_timed_out"] {
                exact["outcomes"][0].as_object_mut().unwrap().remove(field);
            }
        }
        let mut duplicate = exact.clone();
        duplicate["outcomes"]
            .as_array_mut()
            .unwrap()
            .push(exact["outcomes"][0].clone());
        refuse("duplicate terminal identity", &duplicate)?;
        let mut aborted = exact.clone();
        aborted["outcomes"][0]["aborted"] = json!(true);
        refuse("aborted terminal outcome", &aborted)?;
        let mut unknown = exact.clone();
        unknown["unrecognized"] = json!(true);
        refuse("unknown document field", &unknown)?;
        let mut unknown = exact;
        unknown["outcomes"][0]["unrecognized"] = json!(true);
        refuse("unknown outcome field", &unknown)?;
    }
    let mut relabelled = document.clone();
    relabelled["schema"] = json!(1);
    refuse("schema-2 fields relabelled as schema 1", &relabelled)?;
    fs::write(&path, b"{").map_err(|error| error.to_string())?;
    if load_retained_runner_evidence(&results).is_ok() {
        return Err("typed termination reader accepted malformed JSON".into());
    }

    // Preserve the exact historical interpretation, including the old substring
    // behavior for a signal reason that says no timeout occurred. This reader
    // neither rewrites the file nor turns it into current typed evidence.
    let historical = json!({
        "schema": 1, "scheduler_passes": 1,
        "outcomes": [
            {"tag":"cell.legacy-wall", "ok":false, "duration_s":1.0, "returncode":124, "reason":"TIMEOUT >1s", "aborted":false},
            {"tag":"cell.legacy-oom", "ok":false, "duration_s":1.0, "returncode":137, "reason":"OOM-KILLED", "aborted":false},
            {"tag":"cell.legacy-signal", "ok":false, "duration_s":1.0, "returncode":-11, "reason":"received SIGSEGV with no validate timeout, pids guard, or child-cgroup OOM recorded", "aborted":false}
        ]
    });
    let historical_bytes =
        serde_json::to_vec_pretty(&historical).map_err(|error| error.to_string())?;
    fs::write(&path, &historical_bytes).map_err(|error| error.to_string())?;
    let historical_rows =
        load_retained_runner_evidence(&results)?.ok_or("historical evidence disappeared")?;
    let expected = BTreeMap::from([
        (
            "cell.legacy-wall".into(),
            RunnerEvidence {
                seen: true,
                ok: false,
                timed_out: true,
                oom: false,
            },
        ),
        (
            "cell.legacy-oom".into(),
            RunnerEvidence {
                seen: true,
                ok: false,
                timed_out: false,
                oom: true,
            },
        ),
        (
            "cell.legacy-signal".into(),
            RunnerEvidence {
                seen: true,
                ok: false,
                timed_out: true,
                oom: false,
            },
        ),
    ]);
    if historical_rows != expected
        || fs::read(&path).map_err(|error| error.to_string())? != historical_bytes
    {
        return Err("schema-1 evidence bytes or historical interpretation changed".into());
    }
    Ok(())
}

fn direct_scheduler_self_test(scratch: &Path) -> Result<(), String> {
    retained_termination_self_test(scratch)?;
    const CONTROL_STEP_TIMEOUT_SECONDS: i64 = 5;
    const CELL_WALL_TIMEOUT_SECONDS: i64 = 30;
    const CELL_CPU_TIMEOUT_SECONDS: i64 = 5;
    const CELL_COUNT: usize = 20;
    const MANIFEST_GUEST_CAP: usize = 4;

    let direct = scratch.join("direct-scheduler");
    fs::create_dir_all(&direct)
        .map_err(|error| format!("cannot create direct-scheduler fixture: {error}"))?;
    let build_count = direct.join("build-count");
    let executed = direct.join("executed");
    fs::create_dir_all(&executed)
        .map_err(|error| format!("cannot create direct-scheduler outputs: {error}"))?;

    let mut steps = vec![json!({
        "group": "build",
        "job": "shared",
        "cmd": format!("printf 'build\\n' >> {}", shell_quote(&build_count.to_string_lossy())),
        "deps": [],
        "timeout": CONTROL_STEP_TIMEOUT_SECONDS,
        "cpu_timeout": CONTROL_STEP_TIMEOUT_SECONDS,
        "hint": {"hard_mem_max_bytes": 67108864}
    })];
    let mut cell_tags = Vec::new();
    for number in 1..=CELL_COUNT {
        let tag = format!("cell.run-{number:02}");
        cell_tags.push(tag.clone());
        let output = executed.join(format!("{number:02}"));
        let status = if matches!(number, 3 | 17) { 1 } else { 0 };
        // The sleep keeps multiple cells resident so this exercises the
        // manifest_guest cap. Wall latency is not the invariant: a loaded host
        // has delayed these 50 ms fixtures for more than 11 seconds before they
        // still completed. This nested scheduler is unboxed today, so its CPU
        // budget is declared but not enforced; the 30-second wall budget is the
        // effective finite guard while leaving headroom for those observed
        // dispatch stalls.
        steps.push(json!({
            "group": "cell",
            "job": format!("run-{number:02}"),
            "cmd": format!(
                "sleep 0.05; printf 'done\\n' > {}; exit {status}",
                shell_quote(&output.to_string_lossy())
            ),
            "deps": ["build.shared"],
            "timeout": CELL_WALL_TIMEOUT_SECONDS,
            "cpu_timeout": CELL_CPU_TIMEOUT_SECONDS,
            "hint": {
                "resources": {"manifest_guest": 1},
                "hard_mem_max_bytes": 67108864
            }
        }));
    }
    steps.push(json!({
        "group": "pressure",
        "job": "summarize",
        "cmd": "true",
        "deps": cell_tags,
        "timeout": CONTROL_STEP_TIMEOUT_SECONDS,
        "cpu_timeout": CONTROL_STEP_TIMEOUT_SECONDS,
        "hint": {"hard_mem_max_bytes": 67108864}
    }));
    let dag = dag_from_json(
        &serde_json::to_string(&json!({
            "description": "pressure direct scheduler self-test",
            "resource_caps": {"manifest_guest": MANIFEST_GUEST_CAP},
            "steps": steps,
        }))
        .map_err(|error| format!("cannot serialize direct-scheduler fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot parse direct-scheduler fixture: {error}"))?;

    let jobs = default_jobs();
    let effective_cell_width = usize::try_from(jobs)
        .unwrap_or(1)
        .clamp(1, MANIFEST_GUEST_CAP);
    let cell_waves = CELL_COUNT.div_ceil(effective_cell_width);
    // The scheduler correctly refuses a step unless its complete declared
    // budget fits inside the remaining whole-run bound. Account for the build,
    // every resource-capped cell wave, and the summary, then retain one step
    // budget for scheduler/process bookkeeping instead of relying on the
    // fixture's current 50 ms commands.
    let declared_critical_path_seconds = CONTROL_STEP_TIMEOUT_SECONDS * 2
        + CELL_WALL_TIMEOUT_SECONDS * i64::try_from(cell_waves).unwrap();
    let run_timeout_seconds =
        declared_critical_path_seconds + CONTROL_STEP_TIMEOUT_SECONDS;
    let retained_results = direct.clone();
    let execution = with_runner_log_dir(&direct, || {
        with_execution_root(scratch, || {
            execute_typed_dag(
                &dag,
                jobs,
                None,
                Instant::now(),
                run_timeout_seconds,
            )
        })
    })?;
    let cell_outcomes: Vec<_> = execution
        .outcomes
        .iter()
        .filter(|outcome| outcome.tag.starts_with("cell."))
        .collect();
    if cell_outcomes.len() != CELL_COUNT
        || cell_outcomes.iter().filter(|outcome| outcome.ok).count() != 18
        || execution.passes < 2
        || execution.scheduler_wall_s <= 0.0
        || execution.step_profile_rows.is_empty()
    {
        return Err(format!(
            "direct scheduler did not retain all terminal cells across failures: cells={} passes={} ok={} wall={:.3} profile_rows={}",
            cell_outcomes.len(),
            execution.passes,
            cell_outcomes.iter().filter(|outcome| outcome.ok).count(),
            execution.scheduler_wall_s,
            execution.step_profile_rows.len(),
        ));
    }
    let executed_count = fs::read_dir(&executed)
        .map_err(|error| format!("cannot read direct-scheduler outputs: {error}"))?
        .filter_map(Result::ok)
        .count();
    let builds = fs::read_to_string(&build_count)
        .map_err(|error| format!("cannot read direct-scheduler build count: {error}"))?
        .lines()
        .count();
    if executed_count != CELL_COUNT || builds != 1 {
        return Err(format!(
            "direct scheduler did not share one build across {CELL_COUNT} cells: outputs={executed_count} builds={builds}"
        ));
    }

    let evidence = retain_execution_evidence(&retained_results, &execution)?;
    let retained_document: JsonValue = serde_json::from_str(
        &fs::read_to_string(retained_results.join("runner-profile.json"))
            .map_err(|error| format!("cannot read retained scheduler document: {error}"))?,
    )
    .map_err(|error| format!("cannot parse retained scheduler document: {error}"))?;
    if retained_document["scheduler_wall_s"].as_f64() != Some(execution.scheduler_wall_s)
        || retained_document["step_profile_rows"]
            .as_array()
            .is_none_or(Vec::is_empty)
        || !direct.join("runner-profile/journal.jsonl").is_file()
    {
        return Err("retained scheduler calibration evidence is incomplete".into());
    }
    let loaded = load_retained_runner_evidence(&retained_results)?
        .ok_or("typed scheduler outcome file was not loadable")?;
    if evidence.len() != 20
        || evidence.keys().collect::<Vec<_>>() != loaded.keys().collect::<Vec<_>>()
        || evidence.values().any(|row| !row.output_log_available)
        || loaded.values().any(|row| !row.output_log_available)
    {
        return Err(
            "typed scheduler outcome retention changed cell identities or lost output-log availability"
                .into(),
        );
    }

    let legacy_results = direct.join("legacy-retained");
    fs::create_dir_all(&legacy_results)
        .map_err(|error| format!("cannot create legacy runner-evidence fixture: {error}"))?;
    fs::write(
        legacy_results.join("runner-outcomes.json"),
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "scheduler_passes": 1,
            "outcomes": [{
                "tag": "cell.legacy",
                "ok": false,
                "duration_s": 1.0,
                "returncode": 1,
                "reason": "exit 1",
                "aborted": false
            }]
        }))
        .map_err(|error| format!("cannot serialize legacy runner-evidence fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write legacy runner-evidence fixture: {error}"))?;
    let legacy = load_retained_runner_evidence(&legacy_results)?
        .ok_or("legacy runner-evidence fixture was not loadable")?;
    if !legacy.get("cell.legacy").is_some_and(|row| {
        !row.output_log_available
            && row.environmental_block_observation == EnvBlockObservation::NothingObserved
            && runner_output_log("cell.legacy", row.output_log_available).is_none()
    }) {
        return Err("schema-1 runner evidence invented an output log or an observation".into());
    }

    let observation_results = direct.join("observation-retained");
    let observation_output = observation_results.join(RUNNER_STEP_OUTPUT_DIR);
    fs::create_dir_all(&observation_output)
        .map_err(|error| format!("cannot create observation fixture: {error}"))?;
    let observation_rows = [
        ("cell.empty", ""),
        ("cell.ordinary", "ordinary guest failure\n"),
        (
            "cell.banner",
            include_str!("testdata/bpfjailer-pytest-denial.log"),
        ),
        ("cell.fs", "Enforcer: FS, Reason: PATH\n"),
        ("cell.exec", "Enforcer: EXEC, Reason: EXECVE\n"),
        ("cell.net", "Enforcer: NET, Reason: CONNECT\n"),
    ];
    for (tag, output) in observation_rows {
        fs::write(
            observation_output.join(format!("{}.log", sanitize_step_tag(tag))),
            output,
        )
        .map_err(|error| format!("cannot write observation fixture for {tag}: {error}"))?;
    }
    let retained_row = |tag: &str| {
        json!({
            "tag": tag,
            "ok": false,
            "duration_s": 1.0,
            "returncode": 1,
            "reason": "exit 1",
            "aborted": false,
            "oomed": false, "oom_kills": 0, "timed_out": false, "cpu_timed_out": false,
            "output_log": PathBuf::from(RUNNER_STEP_OUTPUT_DIR)
                .join(format!("{}.log", sanitize_step_tag(tag)))
        })
    };
    fs::write(
        observation_results.join("runner-outcomes.json"),
        serde_json::to_string_pretty(&json!({
            "schema": 3,
            "scheduler_passes": 1,
            "outcomes": [
                retained_row("cell.empty"),
                retained_row("cell.ordinary"),
                retained_row("cell.banner"),
                retained_row("cell.fs"),
                retained_row("cell.exec"),
                retained_row("cell.net")
            ]
        }))
        .map_err(|error| format!("cannot serialize observation fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write observation fixture: {error}"))?;
    let observations = load_retained_runner_evidence(&observation_results)?
        .ok_or("schema-3 observation fixture was not loadable")?;
    if !observations.get("cell.empty").is_some_and(|row| {
        row.output_log_available
            && row.environmental_block_observation == EnvBlockObservation::NothingObserved
            && runner_output_log("cell.empty", row.output_log_available)
                == Some(PathBuf::from(RUNNER_STEP_OUTPUT_DIR).join("cell.empty.log"))
    }) || !observations.get("cell.ordinary").is_some_and(|row| {
        row.output_log_available
            && row.environmental_block_observation == EnvBlockObservation::NoDenial
    }) || ["cell.banner", "cell.fs", "cell.exec", "cell.net"]
        .iter()
        .any(|tag| {
            !observations.get(*tag).is_some_and(|row| {
                row.output_log_available
                    && row.environmental_block_observation
                        == EnvBlockObservation::Denied(EnvBlockClass::BpfjailerBanner)
                    && classify_result(
                        *row,
                        Some(1),
                        "FAIL",
                        true,
                        Some("1 failed"),
                        "verify",
                        Some("no_result"),
                        false,
                        false,
                    ) == "sandbox-denied"
            })
        })
        || classify_result(
            *observations.get("cell.empty").expect("fixture row"),
            Some(1),
            "FAIL",
            true,
            Some("1 failed"),
            "verify",
            Some("no_result"),
            false,
            false,
        ) == "sandbox-denied"
        || classify_result(
            *observations.get("cell.ordinary").expect("fixture row"),
            Some(1),
            "FAIL",
            true,
            Some("1 failed"),
            "verify",
            Some("no_result"),
            false,
            false,
        ) == "sandbox-denied"
    {
        return Err(
            "schema-3 runner evidence collapsed BPF/FS/EXEC/NET, ordinary, or no-output observations"
                .into(),
        );
    }

    let bad_path_results = direct.join("bad-path-retained");
    fs::create_dir_all(&bad_path_results)
        .map_err(|error| format!("cannot create bad-path fixture: {error}"))?;
    fs::write(
        bad_path_results.join("runner-outcomes.json"),
        serde_json::to_string_pretty(&json!({
            "schema": 3,
            "scheduler_passes": 1,
            "outcomes": [{
                "tag": "cell.bad-path",
                "ok": false,
                "duration_s": 1.0,
                "returncode": 1,
                "reason": "exit 1",
                "aborted": false,
            "oomed": false, "oom_kills": 0, "timed_out": false, "cpu_timed_out": false,
                "output_log": "elsewhere.log"
            }]
        }))
        .map_err(|error| format!("cannot serialize bad-path fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write bad-path fixture: {error}"))?;
    match load_retained_runner_evidence(&bad_path_results) {
        Err(error) if error.contains("names unexpected output log") => {}
        other => {
            return Err(format!(
                "schema-3 unexpected output-log path did not fail for that reason: {other:?}"
            ));
        }
    }
    let missing_log_results = direct.join("missing-log-retained");
    fs::create_dir_all(&missing_log_results)
        .map_err(|error| format!("cannot create missing-log fixture: {error}"))?;
    fs::write(
        missing_log_results.join("runner-outcomes.json"),
        serde_json::to_string_pretty(&json!({
            "schema": 3,
            "scheduler_passes": 1,
            "outcomes": [retained_row("cell.missing")]
        }))
        .map_err(|error| format!("cannot serialize missing-log fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write missing-log fixture: {error}"))?;
    match load_retained_runner_evidence(&missing_log_results) {
        Err(error) if error.contains("lost stdout/stderr") => {}
        other => {
            return Err(format!(
                "schema-3 missing output log did not fail for that reason: {other:?}"
            ));
        }
    }

    let captured_results = direct.join("captured-bpfjailer");
    let captured_output_dir = captured_results.join(RUNNER_STEP_OUTPUT_DIR);
    fs::create_dir_all(&captured_output_dir)
        .map_err(|error| format!("cannot create captured BPFJailer fixture: {error}"))?;
    let captured_tag = "cell.captured-bpfjailer";
    fs::write(
        captured_output_dir.join(format!("{}.log", sanitize_step_tag(captured_tag))),
        include_str!("testdata/bpfjailer-pytest-denial.log"),
    )
    .map_err(|error| format!("cannot retain captured BPFJailer output: {error}"))?;
    let captured_execution = ExecutionEvidence {
        outcomes: vec![StepOutcome::failed(
            captured_tag.into(),
            1.19,
            "1 failed, 20 passed in 3.52s".into(),
            Some(1),
            false,
            0,
            false,
            30,
            false,
            0,
            0,
            DEFAULT_CPU_TIMEOUT_MULTIPLIER,
            "",
            false,
            Some(21),
            Some(0),
        )],
        passes: 0,
        scheduler_wall_s: 1.19,
        step_profile_rows: Vec::new(),
    };
    let captured = retain_execution_evidence(&captured_results, &captured_execution)?;
    let captured_runner = captured
        .get(captured_tag)
        .copied()
        .ok_or("captured BPFJailer outcome was not retained")?;
    if captured_runner.environmental_block_observation
        != EnvBlockObservation::Denied(EnvBlockClass::BpfjailerBanner)
        || classify_result(
            captured_runner,
            Some(1),
            "FAIL",
            true,
            Some("1 failed"),
            "verify",
            Some("no_result"),
            false,
            false,
        ) != "sandbox-denied"
    {
        return Err(
            "captured BPFJailer output was relabelled as a timeout, failure, or no-result".into(),
        );
    }

    let retained_path = retained_results.join("runner-outcomes.json");
    fs::write(
        &retained_path,
        serde_json::to_string_pretty(&json!({
            "schema": 1,
            "scheduler_passes": 1,
            "outcomes": [{
                "tag": "cell.historical-timeout",
                "ok": false,
                "duration_s": 1.0,
                "returncode": 124,
                "reason": "TIMEOUT >1s",
                "aborted": false
            }]
        }))
        .map_err(|error| format!("cannot serialize historical outcome fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write historical outcome fixture: {error}"))?;
    let historical = load_retained_runner_evidence(&retained_results)?
        .ok_or("historical typed scheduler outcome file was not loadable")?;
    if !historical
        .get("cell.historical-timeout")
        .is_some_and(|row| row.timed_out && !row.oom)
    {
        return Err("schema-1 scheduler evidence lost its historical interpretation".into());
    }

    fs::write(
        &retained_path,
        serde_json::to_string_pretty(&json!({
            "schema": 2,
            "scheduler_passes": 1,
            "outcomes": [{
                "tag": "cell.missing-cpu-timeout",
                "ok": false,
                "duration_s": 1.0,
                "returncode": 1,
                "oomed": false,
                "oom_kills": 0,
                "timed_out": false,
                "reason": "failure",
                "aborted": false
            }]
        }))
        .map_err(|error| format!("cannot serialize incomplete outcome fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write incomplete outcome fixture: {error}"))?;
    let missing_field = load_retained_runner_evidence(&retained_results)
        .expect_err("schema-2 scheduler evidence accepted a missing cpu_timed_out field");
    if !missing_field.contains("cpu_timed_out") {
        return Err(format!(
            "schema-2 scheduler evidence refused an incomplete row without naming cpu_timed_out: {missing_field}"
        ));
    }

    fs::write(
        &retained_path,
        serde_json::to_string_pretty(&json!({
            "schema": 2,
            "scheduler_passes": 1,
            "outcomes": [{
                "tag": "cell.presentation-only-timeout",
                "ok": false,
                "duration_s": 1.0,
                "returncode": 1,
                "oomed": false,
                "oom_kills": 0,
                "timed_out": false,
                "cpu_timed_out": false,
                "reason": "log text mentioned timeout",
                "aborted": false
            }]
        }))
        .map_err(|error| format!("cannot serialize typed outcome fixture: {error}"))?,
    )
    .map_err(|error| format!("cannot write typed outcome fixture: {error}"))?;
    let presentation_only = load_retained_runner_evidence(&retained_results)?
        .ok_or("typed scheduler outcome fixture was not loadable")?;
    if !presentation_only
        .get("cell.presentation-only-timeout")
        .is_some_and(|row| row.seen && !row.ok && !row.timed_out && !row.oom)
    {
        return Err("schema-2 scheduler evidence classified presentation text as a typed fact".into());
    }

    // Both fixtures declare no CPU budget (cpu_timed_out=false, cpu_timeout=0), so the
    // three CPU-policy arguments are the inert triple: canonical 0, the default
    // multiplier, and no platform label. That keeps `cpu_timeout_policy_suffix` silent
    // and leaves both `reason` strings exactly what they were before the runner grew
    // the arguments — this bracket is about telling a wall timeout from an OOM, not
    // about CPU-budget scaling.
    let mut timeout = StepOutcome::failed(
        "cell.timeout".into(),
        1.0,
        String::new(),
        Some(124),
        false,
        0,
        true,
        1,
        false,
        0,
        0,
        DEFAULT_CPU_TIMEOUT_MULTIPLIER,
        "",
        false,
        None,
        None,
    );
    let mut oom = StepOutcome::failed(
        "cell.oom".into(),
        1.0,
        String::new(),
        Some(137),
        true,
        1,
        false,
        1,
        false,
        0,
        0,
        DEFAULT_CPU_TIMEOUT_MULTIPLIER,
        "",
        false,
        None,
        None,
    );
    timeout.reason = "presentation text with no classification words".into();
    oom.reason = "presentation text with no classification words".into();
    if !outcome_evidence(&timeout).timed_out
        || outcome_evidence(&timeout).oom
        || !outcome_evidence(&oom).oom
        || outcome_evidence(&oom).timed_out
    {
        return Err(format!(
            "typed timeout/OOM outcome classification lost its distinction: timeout={:?} oom={:?}",
            timeout.reason, oom.reason
        ));
    }
    Ok(())
}

fn fixture_host_capabilities() -> BTreeMap<HostCapability, CapabilityVerdict> {
    HostCapability::ALL
        .into_iter()
        .map(|capability| {
            (
                capability,
                CapabilityVerdict {
                    present: true,
                    evidence: "pressure-test self-test fixture".into(),
                },
            )
        })
        .collect()
}

fn fixture_attempt(outcome: &str, status: i32) -> AttemptResult {
    AttemptResult {
        index: "1".into(),
        outcome: outcome.into(),
        error_kind: None,
        status: Some(status),
        signal: None,
        timed_out: false,
        duration_ms: 1,
        cpu_usage_usec: Some(1),
        observation_sha256: None,
        argv: vec!["hermit".into(), "run".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
        stdout: String::new(),
        stderr: String::new(),
        verification_report: None,
        verification_report_sha256: None,
        runtime: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        sabre_path_evidence: None,
        sabre_path_evidence_sha256: None,
        reason: None,
    }
}

fn pressure_timeout_self_test() -> Result<(), String> {
    let key = ("fixture/current".into(), "verify".into(), "ptrace".into());
    let raw = CellBudget { cpu_timeout_seconds: 22, timeout_seconds: 57, attempts: Some(3) };
    let selected = BTreeSet::from([key.clone()]);
    for (cpu, wall, expected_cpu, expected_wall, expected_outer) in [
        (1.0, 1.0, 22, 57, 278),
        (1.5, 1.0, 33, 57, 278),
        (1.0, 1.5, 22, 86, 394),
        (2.0, 3.0, 44, 171, 734),
    ] {
        let policy = PressureTimeoutPolicy { version: 1, cpu_multiplier: cpu, wall_multiplier: wall };
        let budgets = resolve_budgets(BTreeMap::from([(key.clone(), raw.clone())]), policy, &selected)?;
        let budget = &budgets[&key];
        if budget.cpu_timeout_seconds != expected_cpu || budget.timeout_seconds != expected_wall
            || outer_timeout(budget)? != expected_outer
            || pressure_timeout(budget, Some(expected_outer))? != expected_outer
            || pressure_timeout(budget, Some(expected_outer + 1))? != expected_outer
            || preparation_node_timeout(budget)? != expected_wall + 60
        {
            return Err(format!("independent pressure timeout resolution changed for CPU={cpu} wall={wall}"));
        }
        let error = pressure_timeout(budget, Some(expected_outer - 1))
            .expect_err("short caller cap must refuse before launch");
        if !error.contains("refusing before launch") {
            return Err(format!("short caller cap reported the wrong refusal: {error}"));
        }
        let mut many_internal_runs = budget.clone();
        many_internal_runs.attempts = Some(32);
        if outer_timeout(&many_internal_runs)? != expected_outer {
            return Err("internal runs multiplied the aggregate execution timeout".into());
        }
    }
    if MAX_ATTEMPTS_PER_CELL != 2 {
        return Err("pressure lifecycle controls need an explicit review of the changed framework attempt count".into());
    }
    for policy in [
        PressureTimeoutPolicy { version: 2, cpu_multiplier: 1.0, wall_multiplier: 1.0 },
        PressureTimeoutPolicy { version: 1, cpu_multiplier: 0.0, wall_multiplier: 1.0 },
        PressureTimeoutPolicy { version: 1, cpu_multiplier: 1.0, wall_multiplier: f64::NAN },
        PressureTimeoutPolicy { version: 1, cpu_multiplier: 3.0, wall_multiplier: 1.0 },
    ] {
        if resolve_budgets(BTreeMap::from([(key.clone(), raw.clone())]), policy, &selected).is_ok() {
            return Err("malformed or inverted current pressure timeout policy was accepted".into());
        }
    }
    let mut missing_recipe = raw.clone();
    missing_recipe.attempts = None;
    if outer_timeout(&missing_recipe).is_ok() {
        return Err("an unavailable cell acquired a timeout-derived execution recipe".into());
    }
    let mut overflow = raw;
    overflow.timeout_seconds = i64::MAX;
    if outer_timeout(&overflow).is_ok() || preparation_node_timeout(&overflow).is_ok() {
        return Err("overflowing lifecycle arithmetic was accepted".into());
    }
    if require_generated_node_count(99_989, 1, 1, 9)? != 100_000 {
        return Err("generated-node boundary omitted a producer or summary".into());
    }
    for (cells, repetitions) in [(99_990, 1), (1, 100_000), (usize::MAX, 2)] {
        if require_generated_node_count(cells, repetitions, 1, 9).is_ok() {
            return Err("oversized pressure graph reached allocation".into());
        }
    }
    if parse_pressure_scope_timeout(7200, Err(env::VarError::NotPresent))?.is_some()
        || parse_pressure_scope_timeout(7200, Ok("7200".into()))? != Some(7200)
    {
        return Err("valid pressure scope marker was refused".into());
    }
    for raw in ["", "garbage", "0", "-1", "7199", "7201", "9223372036854775808"] {
        if parse_pressure_scope_timeout(7200, Ok(raw.into())).is_ok() {
            return Err(format!("malformed or mismatched scope marker {raw:?} was accepted"));
        }
    }
    {
        use std::os::unix::ffi::OsStringExt;
        if parse_pressure_scope_timeout(7200,
            Err(env::VarError::NotUnicode(std::ffi::OsString::from_vec(vec![0xff])))).is_ok()
        {
            return Err("non-UTF-8 pressure scope marker was accepted".into());
        }
    }
    Ok(())
}

fn pressure_sample_classification_self_test() -> Result<(), String> {
    let sample_counts = |qualifying_passes,
                         terminal_passes,
                         product_failures,
                         infrastructure_failures,
                         prerequisite_failures,
                         no_results,
                         missing_repetitions,
                         retried_repetitions| RepeatedOutcomeCounts {
        expected_repetitions: PROMOTION_REPETITIONS,
        observed_repetitions: PROMOTION_REPETITIONS - missing_repetitions,
        qualifying_passes,
        clean_passes: qualifying_passes,
        terminal_passes,
        product_failures,
        infrastructure_failures,
        prerequisite_failures,
        no_results,
        mixed_repetitions: 0,
        missing_repetitions,
        unknown_history_repetitions: 0,
        retried_repetitions,
    };
    let promotion_cases = [
        (
            sample_counts(10, 10, 0, 0, 0, 0, 0, 0),
            PressureSampleClassification::PromotionCandidate,
        ),
        (
            sample_counts(1, 1, 9, 0, 0, 0, 0, 0),
            PressureSampleClassification::Intermittent,
        ),
        (
            sample_counts(9, 9, 1, 0, 0, 0, 0, 0),
            PressureSampleClassification::Intermittent,
        ),
        (
            sample_counts(9, 10, 0, 0, 0, 0, 0, 1),
            PressureSampleClassification::Intermittent,
        ),
        (
            sample_counts(0, 0, 10, 0, 0, 0, 0, 0),
            PressureSampleClassification::ConfirmedFailing,
        ),
        (
            sample_counts(0, 0, 0, 10, 0, 0, 0, 0),
            PressureSampleClassification::InfrastructureFailure,
        ),
        (
            sample_counts(0, 0, 0, 0, 10, 0, 0, 0),
            PressureSampleClassification::PrerequisiteFailure,
        ),
        (
            sample_counts(0, 0, 0, 0, 0, 10, 0, 0),
            PressureSampleClassification::NoResult,
        ),
        (
            sample_counts(0, 0, 9, 1, 0, 0, 0, 0),
            PressureSampleClassification::Incomplete,
        ),
        (
            sample_counts(9, 9, 0, 0, 0, 0, 1, 0),
            PressureSampleClassification::Incomplete,
        ),
    ];
    if promotion_cases
        .iter()
        .any(|(counts, expected)| classify_pressure_sample(*counts) != *expected)
    {
        return Err(format!(
            "pressure sample classification changed unexpectedly: {promotion_cases:?}"
        ));
    }
    let duplicate_attempt_count = RepeatedOutcomeCounts {
        observed_repetitions: PROMOTION_REPETITIONS,
        terminal_passes: PROMOTION_REPETITIONS + 1,
        qualifying_passes: PROMOTION_REPETITIONS,
        ..sample_counts(0, 0, 0, 0, 0, 0, 0, 0)
    };
    if classify_pressure_sample(duplicate_attempt_count)
        != PressureSampleClassification::Incomplete
    {
        return Err("duplicate attempt accounting produced a promotion candidate".into());
    }
    let all_mixed = RepeatedOutcomeCounts {
        mixed_repetitions: PROMOTION_REPETITIONS,
        ..sample_counts(0, 0, 0, 0, 0, 0, 0, 0)
    };
    if classify_pressure_sample(all_mixed) != PressureSampleClassification::Incomplete {
        return Err("mixed repetition outcomes produced a product classification".into());
    }
    Ok(())
}

fn self_test(root: &Path) -> Result<(), String> {
    // Read the real checked-in scorecard before building synthetic fixtures.
    // A scorecard schema bump must take this consumer offline immediately and
    // cheaply rather than only after the long self-test has run.
    let tracked = load_tracked_cells(root)?;
    if tracked.cells.is_empty() {
        return Err("tracked cells are empty".into());
    }
    safe_ci_scope::self_test()?;
    pressure_timeout_self_test()?;
    if series_run_index("a-cell-repetition-0004") != 4
        || series_run_index("a-cell-with-no-suffix") != 0
    {
        return Err("pressure repetition ordinals no longer match retained result directories".into());
    }
    // A divergence located by an earlier attempt remains an observation even
    // when the terminal retry passes. An attempt that located nothing does not
    // manufacture a divergence observation.
    {
        let path = std::env::temp_dir().join(format!(
            "pressure-divergence-history-{}",
            std::process::id()
        ));
        let diverged = r#"{"schema":4,"attempt":1,"run_id":"r","hermit_sha":"s","source_tree_dirty":false,"test":"t","category":"c","lane":"l","mode":"verify","backend":"ptrace","classification":"required","outcome":"FAIL","first_divergent_record":93,"first_divergent_syscall":37,"first_divergent_scheduler_turn":68,"first_divergent_virtual_nanoseconds":7,"first_divergent_left_message":"INFO detcore: left event","first_divergent_right_message":"INFO detcore: right event","timeout_seconds":15,"duration_ms":100,"argv":["a"],"guest_argv":["g"],"env":{},"cwd":"/","shell_command":"x","attempts":[],"reason":null,"error_kind":null,"artifact_dir":"/retained/runs/r/t-verify-ptrace"}"#;
        let passed = r#"{"schema":4,"attempt":2,"run_id":"r","hermit_sha":"s","source_tree_dirty":false,"test":"t","category":"c","lane":"l","mode":"verify","backend":"ptrace","classification":"required","outcome":"PASS","first_divergent_record":null,"first_divergent_syscall":null,"first_divergent_scheduler_turn":null,"first_divergent_virtual_nanoseconds":null,"first_divergent_left_message":null,"first_divergent_right_message":null,"timeout_seconds":15,"duration_ms":200,"argv":["a"],"guest_argv":["g"],"env":{},"cwd":"/","shell_command":"x","attempts":[],"reason":null,"error_kind":null,"artifact_dir":"/retained/runs/r/t-verify-ptrace-attempt-2"}"#;
        fs::write(&path, format!("{diverged}\n{passed}\n"))
            .map_err(|e| format!("cannot write divergence history fixture: {e}"))?;
        let all = read_result_rows(&path)?;
        let earlier = earlier_attempts_that_located(&all, 2);
        if earlier.len() != 1
            || earlier[0].attempt != 1
            || earlier[0].first_divergent_record != Some(93)
            || earlier[0].first_divergent_left_message.as_deref()
                != Some("INFO detcore: left event")
            || earlier[0].first_divergent_right_message.as_deref()
                != Some("INFO detcore: right event")
        {
            return Err(format!(
                "the diverging first attempt must remain an earlier observation; got {} row(s)",
                earlier.len()
            ));
        }
        let reported = cell_result_after_retries(&all)?;
        if reported.attempt != 2 || reported.outcome != "PASS" {
            return Err("a passing retry must remain the cell's reported row".into());
        }
        let mut product_then_infrastructure = all.clone();
        product_then_infrastructure[1].outcome = "ERROR".into();
        let reported = cell_result_after_retries(&product_then_infrastructure)?;
        if reported.attempt != 1 || reported.outcome != "FAIL" {
            return Err(
                "a product failure must remain the reported result when its retry has an infrastructure error"
                    .into(),
            );
        }
        let both_clean = format!(
            "{}\n{}\n",
            diverged
                .replace(
                    "\"first_divergent_record\":93",
                    "\"first_divergent_record\":null",
                )
                .replace(
                    "\"first_divergent_syscall\":37",
                    "\"first_divergent_syscall\":null",
                )
                .replace(
                    "\"first_divergent_scheduler_turn\":68",
                    "\"first_divergent_scheduler_turn\":null",
                )
                .replace(
                    "\"first_divergent_virtual_nanoseconds\":7",
                    "\"first_divergent_virtual_nanoseconds\":null",
                )
                .replace(
                    "\"first_divergent_left_message\":\"INFO detcore: left event\"",
                    "\"first_divergent_left_message\":null",
                )
                .replace(
                    "\"first_divergent_right_message\":\"INFO detcore: right event\"",
                    "\"first_divergent_right_message\":null",
                ),
            passed
        );
        fs::write(&path, both_clean)
            .map_err(|e| format!("cannot write no-coordinate history fixture: {e}"))?;
        let clean = read_result_rows(&path)?;
        if !earlier_attempts_that_located(&clean, 2).is_empty() {
            return Err("an earlier attempt that located nothing must not be reported".into());
        }
        fs::remove_file(&path)
            .map_err(|e| format!("cannot remove divergence history fixture: {e}"))?;
    }
    // The checked files remain immutable throughout this self-test. Production
    // plan/run still checks at its command boundary before constructing a plan.
    check_scorecard(root)?;
    let checked_scorecard = CheckedScorecard {
        root,
        enforce_host_capabilities: false,
        memory_budget_override: Some(i64::MAX),
    };
    let explicit_null = decode_budgets(
        br#"[{"test":"fixture/test","mode":"chaos","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":90,"attempts":null}]"#,
    )?;
    if explicit_null
        .get(&("fixture/test".into(), "chaos".into(), "ptrace".into()))
        .is_none_or(|budget| budget.attempts.is_some())
    {
        return Err("explicit null chaos attempts must remain unavailable".into());
    }
    let backend_specific = decode_budgets(
        br#"[{"test":"fixture/test","mode":"verify","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":30,"attempts":1},{"test":"fixture/test","mode":"verify","backend":"liteinst","cpu_timeout_seconds":2,"timeout_seconds":15,"attempts":1}]"#,
    )?;
    if backend_specific.len() != 2
        || backend_specific
            .get(&(
                "fixture/test".into(),
                "verify".into(),
                "ptrace".into(),
            ))
            .is_none_or(|budget| budget.timeout_seconds != 30)
        || backend_specific
            .get(&(
                "fixture/test".into(),
                "verify".into(),
                "liteinst".into(),
            ))
            .is_none_or(|budget| budget.timeout_seconds != 15)
    {
        return Err("backend-specific cell timeouts were collapsed together".into());
    }
    for (matrix, expected) in [
        (
            br#"[{"test":"fixture/test","mode":"chaos","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":90}]"#.as_slice(),
            "missing field `attempts`",
        ),
        (
            br#"[{"test":"fixture/test","mode":"verify","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":90,"attempts":null}]"#.as_slice(),
            "no attempt count for non-chaos mode",
        ),
        (
            br#"[{"test":"fixture/test","mode":"verify","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":1801,"attempts":1}]"#.as_slice(),
            "outside 1..=1800",
        ),
        (
            br#"[{"test":"fixture/test","mode":"verify","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":90,"attempts":1},{"test":"fixture/test","mode":"verify","backend":"ptrace","cpu_timeout_seconds":2,"timeout_seconds":91,"attempts":1}]"#.as_slice(),
            "conflicting execution budgets",
        ),
    ] {
        let error = decode_budgets(matrix).expect_err("invalid matrix budget must refuse");
        if !error.contains(expected) {
            return Err(format!(
                "invalid matrix budget refused for the wrong reason: {error:?}; expected {expected:?}"
            ));
        }
    }

    let manifest_budgets = load_budgets(root)?;
    let omitted_naked_runs = manifest_budgets
        .get(&(
            "applications/timed-progress-bar".into(),
            "naked".into(),
            "native".into(),
        ))
        .ok_or("self-test manifest lost applications/timed-progress-bar naked budget")?;
    let explicit_naked_runs = manifest_budgets
        .get(&(
            "determinism-stress-c/producer-consumer".into(),
            "naked".into(),
            "native".into(),
        ))
        .ok_or("self-test manifest lost determinism-stress-c/producer-consumer naked budget")?;
    if omitted_naked_runs.attempts != Some(3) || explicit_naked_runs.attempts != Some(5) {
        return Err(format!(
            "pressure attempt counts diverge from the harness: omitted naked runs={:?} (want 3), explicit naked runs={:?} (want 5)",
            omitted_naked_runs.attempts, explicit_naked_runs.attempts
        ));
    }
    let seeded_chaos = manifest_budgets
        .get(&(
            "determinism-stress/order-violation".into(),
            "chaos".into(),
            "ptrace".into(),
        ))
        .ok_or("self-test manifest lost determinism-stress/order-violation chaos budget")?;
    let unavailable_chaos = manifest_budgets
        .get(&(
            "applications/timed-progress-bar".into(),
            "chaos".into(),
            "ptrace".into(),
        ))
        .ok_or("self-test manifest lost applications/timed-progress-bar chaos budget")?;
    if seeded_chaos.attempts != Some(32) || unavailable_chaos.attempts.is_some() {
        return Err(format!(
            "chaos attemptability diverges from the manifest: seeded={:?} (want 32), no-seed={:?} (want unavailable)",
            seeded_chaos.attempts, unavailable_chaos.attempts
        ));
    }
    let budget = CellBudget {
        cpu_timeout_seconds: 2,
        timeout_seconds: 7,
        attempts: Some(3),
    };
    if legacy_pressure_timeout(&budget, None)? != 47 {
        return Err(format!(
            "timeout derivation changed: expected 47, got {}",
            legacy_pressure_timeout(&budget, None)?
        ));
    }
    if legacy_pressure_timeout(
        &CellBudget {
            cpu_timeout_seconds: 600,
            timeout_seconds: 1800,
            attempts: Some(32),
        },
        None,
    )? != LEGACY_PRESSURE_CELL_TIMEOUT_SECONDS
    {
        return Err("pressure timeout did not cap a long repeated red cell".into());
    }
    if legacy_pressure_timeout(
        &CellBudget {
            cpu_timeout_seconds: 600,
            timeout_seconds: 1800,
            attempts: Some(32),
        },
        Some(37),
    )? != 37
    {
        return Err("exact-cell pressure timeout did not apply the requested tighter cap".into());
    }
    let repeated_selection_contract = CellSelection {
        test: Some("fixture/test".into()),
        mode: Some("verify".into()),
        backend: Some("ptrace".into()),
        repetitions: Some(1),
        ..CellSelection::default()
    };
    validate_repetition_selection(&repeated_selection_contract)
        .map_err(|e| format!("valid repeated selection was refused: {e}"))?;
    let mut prefixed_repetition = repeated_selection_contract.clone();
    prefixed_repetition.run_id_prefix = Some("validate-run_1.pid-2".into());
    validate_repetition_selection(&prefixed_repetition)
        .map_err(|e| format!("valid run-id prefix was refused: {e}"))?;
    let mut invalid_prefix = prefixed_repetition.clone();
    invalid_prefix.run_id_prefix = Some("path/escape".into());
    if validate_repetition_selection(&invalid_prefix).is_ok() {
        return Err("run-id prefix accepted a path separator".into());
    }
    let mut prefix_without_repetition = prefixed_repetition;
    prefix_without_repetition.repetitions = None;
    if validate_repetition_selection(&prefix_without_repetition).is_ok() {
        return Err("run-id prefix was accepted without repeated-cell evidence".into());
    }
    let mut two_repetitions = repeated_selection_contract.clone();
    two_repetitions.repetitions = Some(2);
    validate_repetition_selection(&two_repetitions)
        .map_err(|e| format!("two repeated checks were refused: {e}"))?;
    let mut exact_green_repetitions = repeated_selection_contract.clone();
    exact_green_repetitions.green = true;
    validate_repetition_selection(&exact_green_repetitions)
        .map_err(|e| format!("explicit exact green repetition was refused: {e}"))?;
    if CellSelection::default().scheduler_jobs() != default_jobs() {
        return Err("pressure scheduler default diverged from the host-adaptive validate policy".into());
    }
    let mut jobs_args = vec![
        "--results".to_string(),
        "ignored/compat-envelope/jobs-self-test".to_string(),
        "--jobs".to_string(),
        "7".to_string(),
    ]
    .into_iter();
    let (_, _, jobs_selection) = result_options(root, &mut jobs_args, false, true)?;
    if jobs_selection.scheduler_jobs() != 7 {
        return Err("--jobs did not reach the typed scheduler selection".into());
    }
    for invalid in ["0", "not-a-number"] {
        let mut invalid_args = vec![
            "--results".to_string(),
            "ignored/compat-envelope/jobs-self-test".to_string(),
            "--jobs".to_string(),
            invalid.to_string(),
        ]
        .into_iter();
        if result_options(root, &mut invalid_args, false, true).is_ok() {
            return Err(format!("invalid --jobs {invalid:?} was accepted"));
        }
    }
    let mut exact_red_iteration = repeated_selection_contract.clone();
    exact_red_iteration.repetitions = None;
    let sampled_red_batch = CellSelection {
        sample: Some(1),
        seed: Some(7),
        ..CellSelection::default()
    };
    if !exact_red_iteration.allows_dirty_source()
        || CellSelection::default().allows_dirty_source()
        || sampled_red_batch.allows_dirty_source()
        || repeated_selection_contract.allows_dirty_source()
    {
        return Err(
            "dirty-source permission is not limited to one exact red-cell iteration".into(),
        );
    }
    for (label, mut invalid) in [
        ("zero repetitions", repeated_selection_contract.clone()),
        ("partial exact cell", repeated_selection_contract.clone()),
        ("sample", repeated_selection_contract.clone()),
    ] {
        match label {
            "zero repetitions" => invalid.repetitions = Some(0),
            "partial exact cell" => invalid.test = None,
            "sample" => invalid.sample = Some(1),
            _ => unreachable!(),
        }
        if validate_repetition_selection(&invalid).is_ok() {
            return Err(format!("repeated selection accepted {label}"));
        }
    }
    let batch_without_liteinst: BTreeSet<_> = REQUIRED_BUILD_TAGS
        .into_iter()
        .filter(|tag| *tag != "build.liteinst_runtime_release")
        .collect();
    let native_exact = BTreeSet::from([
        "pre.submodules", "pre.reverie_pin", "build.rust_scripts", "setup.manifest_plan",
    ]);
    let mut lean_exact = native_exact.clone();
    lean_exact.extend(["gate.manifest", "build.runtime_release"]);
    let exact_runtime_backends_ok = ["ptrace", "kvm", "dbt", "sabre"]
        .into_iter()
        .all(|backend| required_build_tags(Some(("verify", backend)), false) == lean_exact);
    if !exact_runtime_backends_ok
        || required_build_tags(Some(("naked", "native")), false) != native_exact
        || required_build_tags(Some(("verify", "liteinst")), true)
            != BTreeSet::from(REQUIRED_BUILD_TAGS)
        || required_build_tags(None, false) != batch_without_liteinst
        || required_build_tags(None, true) != BTreeSet::from(REQUIRED_BUILD_TAGS)
    {
        return Err(
            "selected-cell build closure lost a required node or built LiteInst for a sample without LiteInst"
                .into(),
        );
    }
    let non_liteinst_batch =
        selected_cell_dependencies(false, true, "verify", "ptrace", Some("prepare.fixture"));
    let liteinst_batch =
        selected_cell_dependencies(false, true, "verify", "liteinst", Some("prepare.fixture"));
    let exact_repeated =
        selected_cell_dependencies(true, true, "verify", "ptrace", Some("prepare.fixture"));
    if non_liteinst_batch.contains(&"build.liteinst_runtime_release".to_string())
        || !liteinst_batch.contains(&"build.liteinst_runtime_release".to_string())
        || exact_repeated
            != [
                "setup.manifest_plan".to_string(),
                "prepare.fixture".to_string(),
                "build.runtime_release".to_string(),
            ]
        || selected_cell_dependencies(true, false, "naked", "native", None)
            != ["setup.manifest_plan".to_string()]
        || selected_cell_dependencies(true, false, "verify", "ptrace", None)
            != ["setup.manifest_plan".to_string(), "build.runtime_release".to_string()]
        || selected_cell_dependencies(true, false, "verify", "liteinst", None)
            != ["setup.manifest_plan".to_string(), "build.liteinst_runtime_release".to_string()]
    {
        return Err(
            "selected-cell dependencies lost the LiteInst positive/negative build bracket".into(),
        );
    }
    let canonical_build_text = fs::read_to_string(root.join(PORTABLE_DAG))
        .map_err(|e| format!("cannot read canonical build-dependency fixture: {e}"))?;
    let canonical_build_dag = dag_from_json(&canonical_build_text)
        .map_err(|e| format!("cannot parse canonical build-dependency fixture: {e}"))?;
    let all_required_builds = required_build_tags(None, true);
    let mut checked_current_builds = 0usize;
    for canonical_step in canonical_build_dag
        .steps
        .iter()
        .filter(|step| all_required_builds.contains(step.tag().as_str()))
    {
        let mut selected_step = canonical_step.clone();
        retain_required_build_dependencies(&mut selected_step, &all_required_builds)
            .map_err(|e| format!("current canonical build graph was refused: {e}"))?;
        if selected_step.deps != canonical_step.deps {
            return Err(format!("{} lost a canonical prerequisite", selected_step.tag()));
        }
        if selected_step
            .deps
            .iter()
            .any(|dependency| !all_required_builds.contains(dependency.as_str()))
        {
            return Err(format!(
                "{} retained a dependency outside the selected current build graph",
                selected_step.tag()
            ));
        }
        checked_current_builds += 1;
    }
    if checked_current_builds != all_required_builds.len() {
        return Err(format!(
            "current canonical build graph exposed {checked_current_builds}/{} required nodes",
            all_required_builds.len()
        ));
    }
    let mut unexpected_dependency = canonical_build_dag
        .steps
        .iter()
        .find(|step| step.tag() == "build.workspace")
        .ok_or("canonical build graph lost build.workspace")?
        .clone();
    unexpected_dependency
        .deps
        .push("build.unexpected_prerequisite".into());
    let unexpected_error =
        retain_required_build_dependencies(&mut unexpected_dependency, &all_required_builds)
            .expect_err("an unexpected canonical build prerequisite was silently omitted");
    if !unexpected_error.contains("build.workspace")
        || !unexpected_error.contains("build.unexpected_prerequisite")
    {
        return Err(format!(
            "unexpected canonical prerequisite refusal did not name both sides: {unexpected_error}"
        ));
    }
    let probe = "space ' quote";
    let quoted = shell_quote(probe);
    let output = Command::new("bash")
        .args(["-c", &format!("printf '%s' {quoted}")])
        .output()
        .map_err(|e| format!("cannot run quoting bracket: {e}"))?;
    if output.stdout != probe.as_bytes() {
        return Err("shell quoting did not round-trip".into());
    }

    let exact_cell_command = "printf '125\\n' > harness-status; status=0; \
        env HERMIT_BIN=\"$PWD/target/release/hermit\" target/debug/test-harness run \
        --include-manual --test fixture --mode verify \
        --results results.in-progress.jsonl --junit junit.in-progress.xml || status=$?; \
        if test -e results.in-progress.jsonl; then \
        mv -- results.in-progress.jsonl results.jsonl || status=$?; fi; \
        if test -e junit.in-progress.xml; then \
        mv -- junit.in-progress.xml junit.xml || status=$?; fi; \
        printf '%s\\n' \"$status\" > harness-status; exit \"$status\"";
    let fixture_json = json!({
        "resource_caps": {"manifest_guest": 1},
        "steps": [
            {
                "group": "cell",
                "job": "fixture",
                "cmd": exact_cell_command,
                "deps": [],
                "timeout": 20,
                "cpu_timeout": 40,
                "hint": {
                    "resources": {"manifest_guest": 1},
                    "hard_mem_max_bytes": 1024
                }
            },
            {
                "group": "pressure",
                "job": "summarize",
                "cmd": "true",
                "deps": ["cell.fixture"],
                "timeout": 10,
                "cpu_timeout": 10,
                "hint": {"hard_mem_max_bytes": 1024}
            }
        ]
    });
    let fixture_text = serde_json::to_string(&fixture_json)
        .map_err(|e| format!("cannot serialize generated-DAG fixture: {e}"))?;
    let fixture = dag_from_json(&fixture_text)
        .map_err(|e| format!("cannot parse generated-DAG fixture: {e}"))?;
    let fixture_timeouts = BTreeMap::from([("cell.fixture".to_string(), 20)]);
    audit_dag(&fixture, 1, 100, &fixture_timeouts)
        .map_err(|e| format!("positive generated-DAG bracket failed: {e}"))?;
    let fixture_round_trip = dag_from_json(&dag_to_json(&fixture))
        .map_err(|e| format!("cannot reparse generated-DAG fixture: {e}"))?;
    assert_plan_round_trip(&fixture, &fixture_round_trip)
        .map_err(|e| format!("positive generated-DAG round-trip bracket failed: {e}"))?;
    let mut missing_memory_cap = fixture.clone();
    missing_memory_cap.steps[0].hint.hard_mem_max_bytes = None;
    if audit_dag(&missing_memory_cap, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step without a hard memory cap was accepted".into());
    }
    let mut missing_cpu_cap = fixture.clone();
    missing_cpu_cap.steps[0].cpu_timeout = 0;
    if audit_dag(&missing_cpu_cap, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step without an explicit CPU cap was accepted".into());
    }
    let mut missing_resource_cap = fixture.clone();
    missing_resource_cap.resource_caps.remove("manifest_guest");
    if audit_dag(&missing_resource_cap, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step whose named resource has no capacity was accepted".into());
    }
    let mut ungrantable_resource = fixture.clone();
    ungrantable_resource
        .resource_caps
        .insert("manifest_guest".into(), 0);
    if audit_dag(&ungrantable_resource, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step whose named resource demand exceeds capacity was accepted".into());
    }
    let mut widened_cell_timeout = fixture.clone();
    widened_cell_timeout.steps[0].timeout = 21;
    if audit_dag(&widened_cell_timeout, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell wall timeout wider than its selected cap was accepted".into());
    }
    let mut disabled_fixture = fixture.clone();
    disabled_fixture.steps[0].cmd = exact_cell_command
        .replace("--include-manual", "--probe-disabled")
        .replace("--test fixture", "--test fixture --backend kvm");
    audit_dag(&disabled_fixture, 1, 100, &fixture_timeouts)
        .map_err(|e| format!("positive disabled-cell bracket failed: {e}"))?;
    let mut prepared_fixture = fixture.clone();
    prepared_fixture.steps[0].cmd = "printf '125\\n' > '/results/cells/fixture/harness-status'; \
         if ! test \"$(cat '/results/prepare/fixture/status' 2>/dev/null)\" = 0; then \
         printf '126\\n' > '/results/cells/fixture/harness-status'; exit 0; fi; \
         status=0; env target/debug/test-harness run --include-manual --prebuilt \
         --test fixture --mode verify --results results.in-progress.jsonl \
         --junit junit.in-progress.xml || status=$?; \
         mv -- results.in-progress.jsonl results.jsonl || status=$?; \
         printf '%s\\n' \"$status\" > harness-status; exit \"$status\""
        .into();
    audit_dag(&prepared_fixture, 1, 100, &fixture_timeouts)
        .map_err(|e| format!("positive preparation-refusal bracket failed: {e}"))?;
    prepared_fixture.steps[0].cmd = prepared_fixture.steps[0]
        .cmd
        .replace("printf '126", "printf '0");
    if audit_dag(&prepared_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("prebuilt cell without the preparation-failure refusal was accepted".into());
    }
    let mut nested_timeout_fixture = fixture.clone();
    nested_timeout_fixture.steps[0].cmd = exact_cell_command.replace(
        "env HERMIT_BIN",
        "timeout --kill-after=10s 20s env HERMIT_BIN",
    );
    if audit_dag(&nested_timeout_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell with a nested wall timeout was accepted".into());
    }
    let mut swallowed_failure_fixture = fixture.clone();
    swallowed_failure_fixture.steps[0].cmd =
        exact_cell_command.replace("exit \"$status\"", "exit 0");
    if audit_dag(&swallowed_failure_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell command that hid its terminal status was accepted".into());
    }
    let mut direct_result_fixture = fixture.clone();
    direct_result_fixture.steps[0].cmd = exact_cell_command
        .replace("results.in-progress.jsonl", "results.jsonl")
        .replace("mv --", "cp --");
    if audit_dag(&direct_result_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell command without terminal result publication was accepted".into());
    }
    let mut missing_exact_selector = fixture;
    missing_exact_selector.steps[0].cmd = exact_cell_command.replace("--mode verify", "");
    if audit_dag(&missing_exact_selector, 1, 100, &fixture_timeouts).is_ok() {
        return Err("negative generated-DAG bracket accepted a cell without an exact mode".into());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("self-test clock failure: {e}"))?
        .as_nanos();
    let scratch = env::temp_dir().join(format!(
        "hermit-pressure-self-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&scratch).map_err(|e| {
        format!(
            "cannot create self-test directory {}: {e}",
            scratch.display()
        )
    })?;
    let scratch_cleanup = SelfTestDirectory::new(scratch.clone());
    require_empty_result_dir(&scratch)?;

    // Plan-time scorecard validation remains mandatory even though pressure
    // execution no longer recursively runs the full metadata audit. Exercise
    // the actual command boundary with an inert stale-scorecard refusal.
    let stale_scorecard_root = scratch.join("stale-scorecard");
    let stale_scorecard_command = stale_scorecard_root.join("ci/compat-envelope/scorecard.rs");
    fs::create_dir_all(
        stale_scorecard_command
            .parent()
            .expect("scorecard fixture has parent"),
    )
    .map_err(|e| format!("cannot create stale-scorecard fixture: {e}"))?;
    fs::write(&stale_scorecard_command, "#!/bin/sh\nexit 1\n")
        .map_err(|e| format!("cannot write stale-scorecard fixture: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&stale_scorecard_command)
            .map_err(|e| format!("cannot inspect stale-scorecard fixture: {e}"))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&stale_scorecard_command, permissions)
            .map_err(|e| format!("cannot make stale-scorecard fixture executable: {e}"))?;
    }
    if check_scorecard(&stale_scorecard_root).is_ok() {
        return Err("plan-time scorecard check accepted a stale scorecard".into());
    }

    // Batch execution must keep its generated checkout on the host-visible
    // checkout filesystem. Hermit replaces guest /tmp, so silently falling
    // back there would make script-backed cells refuse before execution.
    // Refuse that placement before scheduling.
    let host_tmp = PathBuf::from("/tmp");
    let tmp_source = host_tmp.join(format!(
        "hermit-pressure-host-tmp-self-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&tmp_source)
        .map_err(|e| format!("cannot create host-/tmp checkout fixture: {e}"))?;
    let tmp_source_cleanup = SelfTestDirectory::at(
        tmp_source.clone(),
        host_tmp,
        "hermit-pressure-host-tmp-self-test-",
    );
    command_ok(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&tmp_source),
        "initialize host-/tmp checkout fixture",
    )?;
    fs::write(tmp_source.join(".gitignore"), "/ignored/\n")
        .map_err(|e| format!("cannot write host-/tmp checkout fixture: {e}"))?;
    let tmp_refusal = match FreshCheckout::prepare(&tmp_source, "0") {
        Ok(unexpected) => {
            let path = unexpected.path.clone();
            let cleanup = unexpected.cleanup();
            return Err(match cleanup {
                Ok(()) => format!(
                    "source checkout under host /tmp unexpectedly prepared {}",
                    path.display()
                ),
                Err(cleanup) => format!(
                    "source checkout under host /tmp unexpectedly prepared {}; cleanup also failed: {cleanup}",
                    path.display()
                ),
            });
        }
        Err(error) => error,
    };
    if !tmp_refusal.contains("under host /tmp") {
        return Err(format!(
            "host-/tmp refusal did not name the visibility boundary: {tmp_refusal}"
        ));
    }
    if tmp_source.join("ignored").exists() {
        return Err("host-/tmp refusal created a generated-checkout parent".into());
    }
    tmp_source_cleanup.remove()?;

    // A local clone must also copy objects rather than hard-link them: hard
    // links fail with EXDEV when source and destination are on different
    // filesystems. Exercise the complete generated-checkout front door with a
    // tiny repository on the real checkout filesystem, and prove the copied
    // checkout is detached at the requested commit, usable, and removed only
    // from its recorded parent.
    if !LOCAL_CLONE_ARGS.contains(&"--no-hardlinks") {
        return Err("fresh local clone does not disable object hard links".into());
    }
    let clone_source_parent = root.join("ignored");
    fs::create_dir_all(&clone_source_parent).map_err(|e| {
        format!(
            "cannot create clone self-test parent {}: {e}",
            clone_source_parent.display()
        )
    })?;
    let clone_source = clone_source_parent.join(format!(
        "pressure-clone-self-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&clone_source).map_err(|e| {
        format!(
            "cannot create clone self-test source {}: {e}",
            clone_source.display()
        )
    })?;
    let clone_source_cleanup = SelfTestDirectory::at(
        clone_source.clone(),
        clone_source_parent,
        "pressure-clone-self-test-",
    );
    command_ok(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&clone_source),
        "initialize no-hardlinks clone fixture",
    )?;
    fs::write(clone_source.join(".gitignore"), "/ignored/\n")
        .map_err(|e| format!("cannot write generated-checkout fixture ignore rule: {e}"))?;
    for required in [
        "ci/compat-envelope/pressure-test.rs",
        "agent-utils/rs/dagrun/Cargo.toml",
    ] {
        let path = clone_source.join(required);
        fs::create_dir_all(path.parent().expect("required fixture path has parent"))
            .map_err(|e| format!("cannot create generated-checkout fixture path: {e}"))?;
        fs::write(&path, "fixture\n")
            .map_err(|e| format!("cannot write generated-checkout fixture: {e}"))?;
    }
    fs::write(clone_source.join("tracked"), "usable\n")
        .map_err(|e| format!("cannot write no-hardlinks clone fixture: {e}"))?;
    command_ok(
        Command::new("git")
            .args([
                "add",
                ".gitignore",
                "tracked",
                "ci/compat-envelope/pressure-test.rs",
                "agent-utils/rs/dagrun/Cargo.toml",
            ])
            .current_dir(&clone_source),
        "stage no-hardlinks clone fixture",
    )?;
    command_ok(
        Command::new("git")
            .args([
                "-c",
                "user.name=pressure-test self-test",
                "-c",
                "user.email=pressure-test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ])
            .current_dir(&clone_source),
        "commit no-hardlinks clone fixture",
    )?;
    let clone_sha = git_output(&clone_source, &["rev-parse", "HEAD"])?;
    let blob_sha = git_output(&clone_source, &["rev-parse", "HEAD:tracked"])?;
    if worktree_dirty(&clone_source)? {
        return Err("generated-checkout source fixture is dirty before preparation".into());
    }
    let fresh_fixture = FreshCheckout::prepare(&clone_source, &clone_sha)?;
    let fresh_path = fresh_fixture.path.clone();
    let inspect_fresh = (|| -> Result<(), String> {
        let expected_parent = clone_source.join("ignored");
        if fresh_fixture.parent != expected_parent
            || fresh_path.parent() != Some(expected_parent.as_path())
        {
            return Err(format!(
                "generated checkout used unexpected parent {}",
                fresh_path.display()
            ));
        }
        if worktree_dirty(&clone_source)? {
            return Err("generated checkout made its source fixture dirty".into());
        }
        let cloned_sha = git_output(&fresh_path, &["rev-parse", "HEAD"])?;
        if cloned_sha != clone_sha
            || fs::read_to_string(fresh_path.join("tracked"))
                .ok()
                .as_deref()
                != Some("usable\n")
        {
            return Err(format!(
                "no-hardlinks clone is not an exact usable checkout: expected {clone_sha}, observed {cloned_sha}"
            ));
        }
        if blob_sha.len() < 3 {
            return Err("clone fixture produced a malformed object ID".into());
        }
        let (object_dir, object_name) = blob_sha.split_at(2);
        let source_object = clone_source
            .join(".git/objects")
            .join(object_dir)
            .join(object_name);
        let cloned_object = fresh_path
            .join(".git/objects")
            .join(object_dir)
            .join(object_name);
        let source_object_metadata = fs::metadata(&source_object).map_err(|e| {
            format!(
                "cannot inspect source clone-fixture object {}: {e}",
                source_object.display()
            )
        })?;
        let cloned_object_metadata = fs::metadata(&cloned_object).map_err(|e| {
            format!(
                "cannot inspect copied clone-fixture object {}: {e}",
                cloned_object.display()
            )
        })?;
        {
            use std::os::unix::fs::MetadataExt;
            if source_object_metadata.dev() == cloned_object_metadata.dev()
                && source_object_metadata.ino() == cloned_object_metadata.ino()
            {
                return Err("fresh local clone hard-linked its source object".into());
            }
        }
        Ok(())
    })();
    let cleanup_fresh = fresh_fixture.cleanup();
    match (inspect_fresh, cleanup_fresh) {
        (Ok(()), Ok(())) => {}
        (Err(inspect), Ok(())) => return Err(inspect),
        (Ok(()), Err(cleanup)) => return Err(cleanup),
        (Err(inspect), Err(cleanup)) => {
            return Err(format!(
                "{inspect}; generated-checkout cleanup also failed: {cleanup}"
            ));
        }
    }
    if fresh_path.exists() {
        return Err(format!(
            "generated-checkout cleanup left {} behind",
            fresh_path.display()
        ));
    }
    if worktree_dirty(&clone_source)? {
        return Err("generated-checkout cleanup left its source fixture dirty".into());
    }
    fs::write(scratch.join("old-row"), "stale\n")
        .map_err(|e| format!("cannot write self-test stale row: {e}"))?;
    if require_empty_result_dir(&scratch).is_ok() {
        return Err("nonempty pressure result directory was accepted".into());
    }
    fs::remove_file(scratch.join("old-row"))
        .map_err(|e| format!("cannot remove self-test stale row: {e}"))?;
    prerequisite_scheduler_self_test(&canonical_build_dag, &scratch)?;
    direct_scheduler_self_test(&scratch)?;

    let dirty_fixture = scratch.join("dirty-source");
    fs::create_dir(&dirty_fixture)
        .map_err(|e| format!("cannot create dirty-source fixture: {e}"))?;
    command_ok(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dirty_fixture),
        "initialize dirty-source fixture",
    )?;
    fs::write(dirty_fixture.join("tracked"), "tracked\n")
        .map_err(|e| format!("cannot write dirty-source fixture: {e}"))?;
    command_ok(
        Command::new("git")
            .args(["add", "tracked"])
            .current_dir(&dirty_fixture),
        "stage dirty-source fixture",
    )?;
    command_ok(
        Command::new("git")
            .args([
                "-c",
                "user.name=pressure-test self-test",
                "-c",
                "user.email=pressure-test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ])
            .current_dir(&dirty_fixture),
        "commit dirty-source fixture",
    )?;
    if worktree_dirty(&dirty_fixture)? {
        return Err("clean source was reported dirty".into());
    }
    fs::write(
        dirty_fixture.join(".pressure-test-generated-checkout"),
        "owned marker\n",
    )
    .map_err(|e| format!("cannot write generated-checkout marker fixture: {e}"))?;
    if worktree_dirty(&dirty_fixture)? {
        return Err("the tool-owned generated-checkout marker made clean source dirty".into());
    }
    fs::write(dirty_fixture.join("arbitrary-source"), "untracked\n")
        .map_err(|e| format!("cannot write arbitrary untracked-source fixture: {e}"))?;
    if !worktree_dirty(&dirty_fixture)? {
        return Err("an arbitrary untracked source file was treated as clean".into());
    }

    let unfiltered = pressure_cells(root, &CellSelection::default())?;
    // A NOT-APPLICABLE CELL MUST NEITHER RUN NOR BE SILENTLY IGNORED. This
    // bracket previously keyed on "a red chaos cell without seeds", which was
    // the same population under its old name: before the scorecard could say
    // `not-applicable`, a cell whose backend is not enabled for its mode was
    // recorded as red. The invariant is unchanged -- such a cell must stay out
    // of the executable population, and an exact request for it must be refused
    // WITH THE MANIFEST'S OWN REASON rather than a bare "not red".
    let not_applicable = tracked
        .cells
        .iter()
        .find(|cell| {
            cell.status == "not-applicable"
                && cell.id.mode == "verify"
                && cell.id.backend == "kvm"
        })
        .ok_or("self-test needs at least one disabled KVM verify cell")?
        .clone();
    if not_applicable.not_applicable_reason.is_none() {
        return Err(format!(
            "{}/{}/{} is not-applicable but states no reason",
            not_applicable.id.test, not_applicable.id.mode, not_applicable.id.backend
        ));
    }
    if unfiltered
        .selected
        .iter()
        .any(|tracked| tracked.id == not_applicable.id)
    {
        return Err("a not-applicable cell entered the executable pressure population".into());
    }
    let unavailable_selection = CellSelection {
        test: Some(not_applicable.id.test.clone()),
        mode: Some(not_applicable.id.mode.clone()),
        backend: Some(not_applicable.id.backend.clone()),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let unavailable_error = pressure_cells(root, &unavailable_selection)
        .err()
        .ok_or("an exact not-applicable cell was accepted for execution")?;
    if !unavailable_error.contains("NOT APPLICABLE") {
        return Err(format!(
            "exact not-applicable refusal lost its actionable explanation: {unavailable_error}"
        ));
    }
    let disabled_exact_selection = CellSelection {
        probe_disabled: true,
        ..unavailable_selection.clone()
    };
    let disabled_exact = pressure_cells(root, &disabled_exact_selection)?;
    if disabled_exact.selected.len() != 1
        || disabled_exact.selected[0].id != not_applicable.id
        || disabled_exact.selected[0].enabled
    {
        return Err("explicit disabled-cell probing lost its exact requested cell".into());
    }
    let red_kvm = tracked
        .cells
        .iter()
        .find(|cell| {
            cell.enabled
                && cell.status == "red"
                && cell.id.mode == "verify"
                && cell.id.backend == "kvm"
        })
        .ok_or("self-test needs at least one red KVM verify cell")?;
    let red_as_disabled = CellSelection {
        test: Some(red_kvm.id.test.clone()),
        mode: Some(red_kvm.id.mode.clone()),
        backend: Some(red_kvm.id.backend.clone()),
        probe_disabled: true,
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let red_as_disabled_error = pressure_cells(root, &red_as_disabled)
        .err()
        .ok_or("disabled-cell probe accepted an enabled red cell")?;
    if !red_as_disabled_error.contains("is not a disabled tracked cell") {
        return Err(format!(
            "disabled-cell probe of an enabled red cell reported the wrong error: {red_as_disabled_error}"
        ));
    }
    let absent_kvm = CapabilityVerdict {
        present: false,
        evidence: "planted unavailable /dev/kvm".into(),
    };
    if require_selected_kvm_capability(std::slice::from_ref(red_kvm), &absent_kvm)
        .is_ok()
    {
        return Err("selected KVM cell was accepted with an unavailable host capability".into());
    }
    let present_kvm = CapabilityVerdict {
        present: true,
        evidence: "planted openable /dev/kvm".into(),
    };
    require_selected_kvm_capability(std::slice::from_ref(red_kvm), &present_kvm)
        .map_err(|error| format!("selected KVM cell was refused despite capability: {error}"))?;
    let disabled_batch_selection = CellSelection {
        mode: Some("verify".into()),
        backend: Some("kvm".into()),
        probe_disabled: true,
        repetitions: Some(1),
        run_timeout_seconds: Some(1_000_000),
        ..CellSelection::default()
    };
    let disabled_batch = pressure_cells(root, &disabled_batch_selection)?;
    if disabled_batch.selected.is_empty()
        || disabled_batch.selected.iter().any(|cell| {
            cell.enabled
                || cell.status != "not-applicable"
                || cell.id.mode != "verify"
                || cell.id.backend != "kvm"
        })
    {
        return Err("disabled-backend batch selected an enabled or mismatched cell".into());
    }
    let oversized_disabled_sample = CellSelection {
        sample: Some(disabled_batch.eligible_cells + 1),
        seed: Some(7),
        ..disabled_batch_selection.clone()
    };
    let oversized_disabled_error = pressure_cells(root, &oversized_disabled_sample)
        .err()
        .ok_or("oversized disabled-cell sample was accepted")?;
    if !oversized_disabled_error.contains("disabled cells with executable commands") {
        return Err(format!(
            "oversized disabled-cell sample reported the wrong population: {oversized_disabled_error}"
        ));
    }
    let mut disabled_batch_args = vec![
        "--results".to_string(),
        "ignored/compat-envelope/disabled-batch-self-test".to_string(),
        "--probe-disabled".to_string(),
        "--backend".to_string(),
        "kvm".to_string(),
        "--mode".to_string(),
        "verify".to_string(),
        "--repetitions".to_string(),
        "1".to_string(),
    ]
    .into_iter();
    let (_, _, parsed_disabled_batch) =
        result_options(root, &mut disabled_batch_args, false, true)?;
    if !parsed_disabled_batch.probe_disabled
        || parsed_disabled_batch.backend.as_deref() != Some("kvm")
        || parsed_disabled_batch.mode.as_deref() != Some("verify")
        || parsed_disabled_batch.is_exact()
    {
        return Err("disabled-backend batch options did not retain their selection".into());
    }
    let disabled_batch_results = scratch.join("disabled-batch-plan");
    let (disabled_batch_metadata, disabled_batch_dag) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &disabled_batch_results,
        &disabled_batch_results.join("dag.json"),
        &disabled_batch_selection,
    )?;
    if !disabled_batch_metadata.probe_disabled
        || disabled_batch_dag
            .steps
            .iter()
            .filter(|step| step.group == "cell")
            .any(|step| !step.desc.starts_with("Repeat disabled cell "))
        || disabled_batch_dag
            .steps
            .iter()
            .find(|step| step.group == "pressure" && step.job == "summarize")
            .is_none_or(|step| {
                step.desc
                    != "Wait for every repeated disabled-cell check before reading retained runner evidence"
            })
    {
        return Err("disabled-backend plan lost its population identity".into());
    }
    for step in disabled_batch_dag.steps.iter().filter(|step| step.group == "cell") {
        let manifest = step
            .structured_test_results_manifest()
            .map_err(|error| format!("pressure cell {}: {error}", step.tag()))?
            .ok_or_else(|| {
                format!(
                    "pressure cell {} omitted its structured result declaration",
                    step.tag()
                )
            })?;
        if manifest.owner != step.tag() {
            return Err(format!(
                "pressure cell {} declared owner {:?}", step.tag(), manifest.owner
            ));
        }
    }
    let mut green_backend_args = vec![
        "--results".to_string(),
        "ignored/compat-envelope/green-backend-self-test".to_string(),
        "--green".to_string(),
        "--backend".to_string(),
        "kvm".to_string(),
        "--mode".to_string(),
        "verify".to_string(),
        "--repetitions".to_string(),
        "3".to_string(),
    ]
    .into_iter();
    let (_, _, parsed_green_backend) =
        result_options(root, &mut green_backend_args, false, true)?;
    if !parsed_green_backend.green
        || parsed_green_backend.backend.as_deref() != Some("kvm")
        || parsed_green_backend.mode.as_deref() != Some("verify")
        || parsed_green_backend.is_exact()
    {
        return Err("green-backend batch options did not retain their selection".into());
    }
    for (arguments, expected_error) in [
        (
            vec![
                "--results",
                "ignored/compat-envelope/invalid-selection-self-test",
                "--probe-disabled",
            ],
            "--probe-disabled requires --backend",
        ),
        (
            vec![
                "--results",
                "ignored/compat-envelope/invalid-selection-self-test",
                "--probe-disabled",
                "--backend",
                "kvm",
                "--green",
            ],
            "--probe-disabled and --green are mutually exclusive",
        ),
        (
            vec![
                "--results",
                "ignored/compat-envelope/invalid-selection-self-test",
                "--probe-disabled",
                "--backend",
                "kvm",
                "--test",
                "fixture/test",
            ],
            "--probe-disabled with --test also requires --mode",
        ),
    ] {
        let mut arguments = arguments.into_iter().map(str::to_string);
        let error = result_options(root, &mut arguments, false, true)
            .err()
            .ok_or("invalid disabled-backend selection was accepted")?;
        if !error.contains(expected_error) {
            return Err(format!(
                "invalid disabled-backend selection reported the wrong error: {error}"
            ));
        }
    }
    for arguments in [
        vec![
            "--results",
            "ignored/compat-envelope/invalid-selection-self-test",
            "--green",
            "--repetitions",
            "3",
            "--test",
            "fixture/test",
        ],
        vec![
            "--results",
            "ignored/compat-envelope/invalid-selection-self-test",
            "--green",
            "--repetitions",
            "3",
            "--test",
            "fixture/test",
            "--backend",
            "kvm",
        ],
    ] {
        let mut arguments = arguments.into_iter().map(str::to_string);
        let error = result_options(root, &mut arguments, false, true)
            .err()
            .ok_or("incomplete exact green selection was accepted")?;
        if !error.contains("an exact-cell selection requires --test, --mode, and --backend") {
            return Err(format!(
                "incomplete exact green selection reported the wrong error: {error}"
            ));
        }
    }
    let exact_id = unfiltered
        .selected
        .iter()
        .find(|tracked| tracked.id.backend == "ptrace" && tracked.id.mode == "verify")
        .or_else(|| unfiltered.selected.first())
        .ok_or("self-test needs at least one red compatibility cell")?
        .id
        .clone();
    let exact_selection = CellSelection {
        test: Some(exact_id.test.clone()),
        mode: Some(exact_id.mode.clone()),
        backend: Some(exact_id.backend.clone()),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let exact_results = scratch.join("exact-plan");
    let (exact_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &exact_results,
        &exact_results.join("dag.json"),
        &exact_selection,
    )?;
    if exact_metadata.cells != [exact_id.clone()] {
        return Err("generated exact-cell plan did not retain exactly its requested cell".into());
    }
    let mut old_schema_metadata = exact_metadata.clone();
    old_schema_metadata.source_tree_dirty = false;
    old_schema_metadata.eligible_cells = 0;
    validate_run_contract(root, &exact_results, &old_schema_metadata, false)
        .map_err(|e| format!("schema-3 run without repetitions changed behavior: {e}"))?;
    if old_schema_metadata.repetitions.is_some()
        || cell_run_slug(&old_schema_metadata.cells[0], None)
            != base_cell_slug(&old_schema_metadata.cells[0])
    {
        return Err("schema-3 run without repetitions changed its retained cell path".into());
    }

    let tracked_text = fs::read_to_string(root.join(TRACKED_CELLS))
        .map_err(|e| format!("cannot read tracked cells for repetition bracket: {e}"))?;
    let tracked: TrackedCells = serde_json::from_str(&tracked_text)
        .map_err(|e| format!("cannot parse tracked cells for repetition bracket: {e}"))?;
    let expected_red_ids: BTreeSet<_> = unfiltered
        .selected
        .iter()
        .map(|tracked| tracked.id.clone())
        .collect();
    let expected_unavailable_red_ids: BTreeSet<_> = unfiltered
        .unavailable
        .iter()
        .map(|tracked| tracked.id.clone())
        .collect();
    let non_kvm_cells_file_id = expected_red_ids
        .iter()
        .find(|cell| cell.mode == "verify" && cell.backend != "kvm")
        .cloned()
        .ok_or("self-test needs one executable non-KVM red verify cell for --cells-file")?;
    let mut cells_file_ids = vec![red_kvm.id.clone(), non_kvm_cells_file_id];
    cells_file_ids.sort();
    let cells_file_path = scratch.join("selected-cells.jsonl");
    let cells_file_text = canonical_cells_jsonl(&cells_file_ids)?;
    fs::write(&cells_file_path, &cells_file_text)
        .map_err(|error| format!("cannot write --cells-file self-test fixture: {error}"))?;
    let cells_file_digest = format!("{:x}", Sha256::digest(cells_file_text.as_bytes()));
    let cells_population_digest = selected_population_sha256(&cells_file_ids)?;

    let mut missing_repetitions_args = vec![
        "--results".to_string(),
        scratch.join("missing-repetitions").to_string_lossy().into_owned(),
        "--cells-file".to_string(),
        cells_file_path.to_string_lossy().into_owned(),
    ]
    .into_iter();
    let missing_repetitions_error = result_options(
        root,
        &mut missing_repetitions_args,
        false,
        true,
    )
    .err()
    .ok_or("--cells-file without --repetitions was accepted")?;
    if !missing_repetitions_error.contains("--cells-file requires --repetitions") {
        return Err(format!(
            "--cells-file without --repetitions reported the wrong error: {missing_repetitions_error}"
        ));
    }

    let duplicate_cells_file_path = scratch.join("duplicate-selected-cells.jsonl");
    fs::write(
        &duplicate_cells_file_path,
        canonical_cells_jsonl(&[
            cells_file_ids[0].clone(),
            cells_file_ids[0].clone(),
        ])?,
    )
    .map_err(|error| format!("cannot write duplicate --cells-file fixture: {error}"))?;
    let duplicate_error = load_cells_file(&duplicate_cells_file_path)
        .err()
        .ok_or("duplicate --cells-file identity was accepted")?;
    if !duplicate_error.contains("repeats") {
        return Err(format!(
            "duplicate --cells-file identity reported the wrong error: {duplicate_error}"
        ));
    }

    let noncanonical_cells_file_path = scratch.join("noncanonical-selected-cells.jsonl");
    let noncanonical = format!(
        "{{\"lane\":{},\"category\":{},\"test\":{},\"mode\":{},\"backend\":{}}}\n",
        serde_json::to_string(&cells_file_ids[0].lane).unwrap(),
        serde_json::to_string(&cells_file_ids[0].category).unwrap(),
        serde_json::to_string(&cells_file_ids[0].test).unwrap(),
        serde_json::to_string(&cells_file_ids[0].mode).unwrap(),
        serde_json::to_string(&cells_file_ids[0].backend).unwrap(),
    );
    fs::write(&noncanonical_cells_file_path, noncanonical)
        .map_err(|error| format!("cannot write noncanonical --cells-file fixture: {error}"))?;
    if load_cells_file(&noncanonical_cells_file_path)
        .is_ok()
    {
        return Err("noncanonical --cells-file identity was accepted".into());
    }

    let mut unmatched_id = cells_file_ids[0].clone();
    unmatched_id.test.push_str("-not-in-scorecard");
    let unmatched_cells_file_path = scratch.join("unmatched-selected-cells.jsonl");
    fs::write(
        &unmatched_cells_file_path,
        canonical_cells_jsonl(&[unmatched_id])?,
    )
    .map_err(|error| format!("cannot write unmatched --cells-file fixture: {error}"))?;
    let unmatched_selection = CellSelection {
        repetitions: Some(1),
        cells_file: Some(unmatched_cells_file_path),
        ..CellSelection::default()
    };
    let unmatched_error = pressure_cells(root, &unmatched_selection)
        .err()
        .ok_or("unmatched --cells-file identity was accepted")?;
    if !unmatched_error.contains("is not present in the tracked scorecard") {
        return Err(format!(
            "unmatched --cells-file identity reported the wrong error: {unmatched_error}"
        ));
    }
    let unsupported_cells_file_path = scratch.join("unsupported-selected-cells.jsonl");
    fs::write(
        &unsupported_cells_file_path,
        canonical_cells_jsonl(std::slice::from_ref(&not_applicable.id))?,
    )
    .map_err(|error| format!("cannot write unsupported --cells-file fixture: {error}"))?;
    let unsupported_selection = CellSelection {
        repetitions: Some(1),
        cells_file: Some(unsupported_cells_file_path),
        ..CellSelection::default()
    };
    let unsupported_error = pressure_cells(root, &unsupported_selection)
        .err()
        .ok_or("unsupported --cells-file identity was accepted")?;
    if !unsupported_error.contains("is unsupported") {
        return Err(format!(
            "unsupported --cells-file identity reported the wrong error: {unsupported_error}"
        ));
    }

    let cells_file_results = scratch.join("cells-file-plan");
    let cells_file_budget_keys = cells_file_ids.iter()
        .map(|cell| (cell.test.clone(), cell.mode.clone(), cell.backend.clone()))
        .collect();
    let cells_file_budgets = resolve_budgets(
        manifest_budgets.clone(), PressureTimeoutPolicy::from_env()?, &cells_file_budget_keys,
    )?;
    let mut cells_file_expected_timeouts = BTreeMap::new();
    for cell in &cells_file_ids {
        let budget = cells_file_budgets.get(&(cell.test.clone(), cell.mode.clone(), cell.backend.clone()))
            .ok_or("cells-file fixture lost its selected budget")?;
        for repetition in 1..=2 {
            cells_file_expected_timeouts.insert(
                format!("cell.{}", cell_run_slug(cell, Some(repetition))), outer_timeout(budget)?,
            );
        }
    }
    let cells_file_declared_cap = *cells_file_expected_timeouts.values().max()
        .ok_or("cells-file fixture has no timeout")?;
    let cells_file_selection = CellSelection {
        cell_timeout_seconds: Some(cells_file_declared_cap),
        repetitions: Some(2),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        jobs: Some(316),
        manifest_guest_cap: Some(2),
        kvm_guest_cap: Some(1),
        cells_file: Some(cells_file_path.clone()),
        ..CellSelection::default()
    };
    let mut ineffective_kvm_cap = cells_file_selection.clone();
    ineffective_kvm_cap.kvm_guest_cap = Some(3);
    if validate_selection_shape(&ineffective_kvm_cap)
        .err()
        .is_none_or(|error| !error.contains("exceeds the effective manifest/scheduler width"))
    {
        return Err("ineffective --kvm-guest-cap was not refused by name".into());
    }
    let non_kvm_tracked = tracked
        .cells
        .iter()
        .find(|cell| cells_file_ids.contains(&cell.id) && cell.id.backend != "kvm")
        .cloned()
        .ok_or("self-test non-KVM identity disappeared from tracked cells")?;
    let non_kvm_explicit_cap = CellSelection {
        repetitions: Some(1),
        jobs: Some(4),
        kvm_guest_cap: Some(1),
        ..CellSelection::default()
    };
    let non_kvm_cap_error = validate_guest_caps_against_selected_demand(
        std::slice::from_ref(&non_kvm_tracked),
        &non_kvm_explicit_cap,
    )
    .err()
    .ok_or("non-KVM selection accepted an ineffective explicit KVM cap")?;
    if !non_kvm_cap_error.contains("effective KVM demand 0")
        || !non_kvm_cap_error.contains("omit --kvm-guest-cap")
    {
        return Err(format!(
            "ineffective non-KVM cap reported the wrong error: {non_kvm_cap_error}"
        ));
    }
    let non_kvm_omitted_cap = CellSelection {
        kvm_guest_cap: None,
        ..non_kvm_explicit_cap.clone()
    };
    validate_selection_shape(&non_kvm_omitted_cap)?;
    validate_guest_caps_against_selected_demand(
        std::slice::from_ref(&non_kvm_tracked),
        &non_kvm_omitted_cap,
    )?;
    let above_total_demand = CellSelection {
        repetitions: Some(1),
        jobs: Some(4),
        manifest_guest_cap: Some(2),
        ..CellSelection::default()
    };
    if validate_guest_caps_against_selected_demand(
        std::slice::from_ref(&non_kvm_tracked),
        &above_total_demand,
    )
    .err()
    .is_none_or(|error| !error.contains("effective demand 1"))
    {
        return Err("manifest guest cap above selected demand was not refused".into());
    }
    let (mut cells_file_metadata, cells_file_dag) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &cells_file_results,
        &cells_file_results.join("dag.json"),
        &cells_file_selection,
    )?;
    let expected_memory = declared_memory_at_manifest_guest_cap(
        &cells_file_dag,
        cells_file_selection.scheduler_jobs(),
        cells_file_selection.manifest_guest_cap(),
        cells_file_selection.kvm_guest_cap(),
    )?;
    let expected_max_safe = max_safe_manifest_guest_effective_width(
        &cells_file_dag,
        cells_file_selection.scheduler_jobs(),
        cells_file_selection.kvm_guest_cap(),
        i64::MAX,
    )?;
    let expected_max_safe_kvm = max_safe_kvm_guest_cap(
        &cells_file_dag,
        cells_file_selection.scheduler_jobs(),
        cells_file_selection.manifest_guest_cap(),
        i64::MAX,
    )?;
    if validate_manifest_guest_memory(
        &cells_file_dag,
        &cells_file_selection,
        Some(expected_memory),
    )? != (
        Some(expected_memory),
        expected_memory,
        Some(expected_max_safe),
        Some(expected_max_safe_kvm),
    )
    {
        return Err("safe explicit manifest_guest cap lost its exact memory calculation".into());
    }
    let unsafe_memory_error = validate_manifest_guest_memory(
        &cells_file_dag,
        &cells_file_selection,
        Some(expected_memory - 1),
    )
    .err()
    .ok_or("unsafe explicit manifest_guest cap was accepted")?;
    if !unsafe_memory_error.contains("highest safe cap for this population")
        || validate_manifest_guest_memory(&cells_file_dag, &cells_file_selection, None).is_ok()
    {
        return Err("explicit manifest_guest cap did not refuse unsafe or unknown memory".into());
    }
    let cells_file_cell_steps: Vec<_> = cells_file_dag
        .steps
        .iter()
        .filter(|step| step.group == "cell")
        .collect();
    let cells_file_timeouts: BTreeMap<_, _> = cells_file_cell_steps
        .iter()
        .map(|step| (step.tag(), step.timeout))
        .collect();
    if cells_file_metadata.cells != cells_file_ids
        || cells_file_metadata.repetitions != Some(2)
        || cells_file_metadata.jobs != 316
        || cells_file_metadata.manifest_guest_cap != 2
        || !cells_file_metadata.manifest_guest_cap_explicit
        || cells_file_metadata.kvm_guest_cap != 1
        || !cells_file_metadata.kvm_guest_cap_explicit
        || cells_file_metadata.manifest_guest_memory_required_bytes != Some(expected_memory)
        || cells_file_metadata.manifest_guest_control_plane_headroom_bytes
            != Some(CONTROL_PLANE_HEADROOM_BYTES)
        || cells_file_metadata.manifest_guest_max_safe_cap != Some(expected_max_safe)
        || cells_file_metadata.kvm_guest_max_safe_cap != Some(expected_max_safe_kvm)
        || cells_file_metadata.cell_timeout_seconds != Some(cells_file_declared_cap)
        || cells_file_metadata.cells_file.as_deref()
            != Some(cells_file_path.to_string_lossy().as_ref())
        || cells_file_metadata.cells_file_sha256.as_deref() != Some(cells_file_digest.as_str())
        || cells_file_metadata.selected_population_sha256.as_deref()
            != Some(cells_population_digest.as_str())
        || cells_file_cell_steps.len() != 4
        || cells_file_cell_steps.iter().any(|step| {
            cells_file_expected_timeouts.get(&step.tag()) != Some(&step.timeout)
                || step.cmd.matches("test-harness run").count() != 1
                || !step.cmd.contains("--mode 'verify'")
                || !step.cmd.contains("E2E_KEEP_VERIFY_LOGS=1")
        })
        || cells_file_dag.resource_caps.get("manifest_guest") != Some(&2)
        || cells_file_dag.resource_caps.get("kvm_guest") != Some(&1)
        || cells_file_cell_steps.iter().any(|step| {
            let is_kvm = step.cmd.contains("--backend 'kvm'");
            (step.hint.resources.get("kvm_guest") == Some(&1)) != is_kvm
        })
    {
        return Err(
            "--cells-file plan lost its exact identities, repetitions, timeout, jobs, digest, or verify-harness contract"
                .into(),
        );
    }
    let kvm_template = cells_file_cell_steps
        .iter()
        .find(|step| step.hint.resources.get("kvm_guest") == Some(&1))
        .ok_or("--cells-file dual-cap fixture lost its KVM cell")?;
    let portable_template = cells_file_cell_steps
        .iter()
        .find(|step| step.hint.resources.get("kvm_guest").copied().unwrap_or(0) == 0)
        .ok_or("--cells-file dual-cap fixture lost its portable cell")?;
    let preparation_template = cells_file_dag
        .steps
        .iter()
        .find(|step| step.group == "prepare")
        .ok_or("--cells-file memory fixture lost preparation")?;
    let mut memory_dag = cells_file_dag.clone();
    memory_dag.steps.clear();
    for number in 0..150 {
        let mut step = (*kvm_template).clone();
        step.job = format!("kvm-{number:04}");
        memory_dag.steps.push(step);
    }
    for number in 0..1340 {
        let mut step = (*portable_template).clone();
        step.job = format!("portable-{number:04}");
        memory_dag.steps.push(step);
    }
    let mut preparation = preparation_template.clone();
    preparation.job = "fixture".into();
    memory_dag.steps.push(preparation);
    let mut liteinst = preparation_template.clone();
    liteinst.group = "build".into();
    liteinst.job = "liteinst_runtime_release".into();
    liteinst.hint.hard_mem_max_bytes = Some(6 * 1024 * 1024 * 1024);
    memory_dag.steps.push(liteinst);
    for step in &mut memory_dag.steps {
        step.deps.clear();
    }
    let gib = 1024_i64 * 1024 * 1024;
    if declared_memory_at_manifest_guest_cap(&memory_dag, 316, 128, 8)? != 498 * gib
        || declared_memory_at_manifest_guest_cap(&memory_dag, 316, 133, 8)? != 513 * gib
        || declared_memory_at_manifest_guest_cap(&memory_dag, 316, 128, 10)? != 524 * gib
        || max_safe_manifest_guest_effective_width(&memory_dag, 316, 8, 512 * gib)? != 132
    {
        return Err(
            "dual manifest/KVM cap memory model lost the 3W + 13K + 10 GiB boundary"
                .into(),
        );
    }
    // A verify cell node intentionally wraps the harness's two executions and
    // comparison. The DAG therefore has one node per identity/repetition, not
    // three. Removing any such wrapper must still fail the plan-shape audit.
    let mut missing_cells_file_repetition = cells_file_dag.clone();
    let removed_job = cells_file_cell_steps[0].job.clone();
    missing_cells_file_repetition
        .steps
        .retain(|step| !(step.group == "cell" && step.job == removed_job));
    if audit_dag(
        &missing_cells_file_repetition,
        4,
        cells_file_metadata.run_timeout_seconds,
        &cells_file_timeouts,
    )
    .is_ok()
    {
        return Err("--cells-file plan audit accepted an omitted repetition".into());
    }
    cells_file_metadata.source_tree_dirty = false;
    validate_run_contract(root, &cells_file_results, &cells_file_metadata, false)
        .map_err(|error| format!("valid retained --cells-file run was refused: {error}"))?;
    let mut uppercase_cells_file_digest = cells_file_metadata.clone();
    uppercase_cells_file_digest.cells_file_sha256 = Some("A".repeat(64));
    if validate_run_contract(
        root,
        &cells_file_results,
        &uppercase_cells_file_digest,
        false,
    )
    .is_ok()
    {
        return Err("retained --cells-file run accepted an uppercase SHA-256".into());
    }
    let mut incomplete_cells_file_metadata = cells_file_metadata.clone();
    incomplete_cells_file_metadata.cells.pop();
    incomplete_cells_file_metadata.eligible_cells = 1;
    let population_mutation_error = validate_run_contract(
        root,
        &cells_file_results,
        &incomplete_cells_file_metadata,
        false,
    )
    .err()
    .ok_or("retained --cells-file run accepted an omitted identity and adjusted count")?;
    if !population_mutation_error.contains("selected-cell population SHA-256 mismatch") {
        return Err(format!(
            "retained --cells-file population mutation reported the wrong error: {population_mutation_error}"
        ));
    }
    let green_id = tracked
        .cells
        .iter()
        .find(|tracked| {
            tracked.enabled
                && tracked.status == "green"
                && tracked.id.mode == "verify"
                && tracked.id.backend == "ptrace"
        })
        .ok_or("self-test needs one enabled green ptrace/verify cell")?
        .id
        .clone();
    let repeated_selection = CellSelection {
        test: Some(exact_id.test.clone()),
        mode: Some(exact_id.mode.clone()),
        backend: Some(exact_id.backend.clone()),
        repetitions: Some(3),
        run_id_prefix: Some("validate-one-pid100".into()),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let repeated_cells = pressure_cells(root, &repeated_selection)?;
    if repeated_cells.selected.len() != 1 || repeated_cells.selected[0].id != exact_id {
        return Err("exact repeated selection did not retain its requested red cell".into());
    }
    let implicit_green_selection = CellSelection {
        test: Some(green_id.test.clone()),
        mode: Some(green_id.mode.clone()),
        backend: Some(green_id.backend.clone()),
        repetitions: Some(3),
        ..CellSelection::default()
    };
    if pressure_cells(root, &implicit_green_selection).is_ok() {
        return Err("repeated red-cell selection accepted an unrequested green cell".into());
    }
    let exact_green_selection = CellSelection {
        green: true,
        ..implicit_green_selection
    };
    let exact_green_cells = pressure_cells(root, &exact_green_selection)?;
    if exact_green_cells.selected.len() != 1 || exact_green_cells.selected[0].id != green_id {
        return Err("explicit --green exact repetition lost its requested green cell".into());
    }

    let repeated_results = scratch.join("repeated-plan");
    let (mut repeated_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &repeated_results,
        &repeated_results.join("dag.json"),
        &repeated_selection,
    )?;
    if repeated_metadata.cells != [exact_id.clone()]
        || repeated_metadata.repetitions != Some(3)
        || repeated_metadata.run_id_prefix.as_deref() != Some("validate-one-pid100")
    {
        return Err("generated repeated plan did not retain one cell and three repetitions".into());
    }
    let mut second_invocation = repeated_selection.clone();
    second_invocation.run_id_prefix = Some("validate-two-pid200".into());
    let second_invocation_results = scratch.join("repeated-plan-second-invocation");
    write_plan_after_scorecard_check(
        &checked_scorecard,
        &second_invocation_results,
        &second_invocation_results.join("dag.json"),
        &second_invocation,
    )?;
    let second_invocation_dag = dag_from_json(
        &fs::read_to_string(second_invocation_results.join("dag.json"))
            .map_err(|e| format!("cannot read second-invocation DAG: {e}"))?,
    )
    .map_err(|e| format!("cannot parse second-invocation DAG: {e}"))?;
    let first_run_ids: BTreeSet<_> = (1..=3)
        .map(|number| {
            cell_evidence_run_id(&exact_id, Some(number), Some("validate-one-pid100"))
        })
        .collect();
    let second_run_ids: BTreeSet<_> = (1..=3)
        .map(|number| {
            cell_evidence_run_id(&exact_id, Some(number), Some("validate-two-pid200"))
        })
        .collect();
    if !first_run_ids.is_disjoint(&second_run_ids)
        || second_invocation_dag.steps.iter().filter(|step| step.group == "cell").any(
            |step| {
                !step
                    .cmd
                    .contains(&format!("E2E_RUN_ID='validate-two-pid200--{}'", step.job))
            },
        )
    {
        return Err("independent repeated-cell invocations reused an evidence run ID".into());
    }
    let mut huge_repetition_selection = repeated_selection.clone();
    huge_repetition_selection.repetitions = Some(usize::MAX);
    huge_repetition_selection.run_timeout_seconds = Some(i64::MAX);
    let huge_repetition_results = scratch.join("huge-repetition-plan");
    let huge_repetition_error = write_plan_after_scorecard_check(
        &checked_scorecard,
        &huge_repetition_results,
        &huge_repetition_results.join("dag.json"),
        &huge_repetition_selection,
    )
    .err()
    .ok_or("an unrepresentable repetition count reached plan allocation")?;
    if huge_repetition_results.exists() || !huge_repetition_error.contains("--repetitions") {
        return Err(format!(
            "huge repetition refusal was late or unactionable: {huge_repetition_error}"
        ));
    }
    let mut too_many_nodes = repeated_selection.clone();
    too_many_nodes.repetitions = Some(100_000);
    too_many_nodes.run_timeout_seconds = Some(i64::MAX);
    let too_many_results = scratch.join("bounded-node-count");
    let error = write_plan_after_scorecard_check(&checked_scorecard, &too_many_results,
        &too_many_results.join("dag.json"), &too_many_nodes)
        .err().ok_or("oversized representable plan reached allocation")?;
    if too_many_results.exists() || !error.contains("100000-node safety bound") {
        return Err(format!("node-count refusal was late or unrelated: {error}"));
    }
    let repeated_dag_text = fs::read_to_string(repeated_results.join("dag.json"))
        .map_err(|e| format!("cannot read repeated-plan DAG: {e}"))?;
    let repeated_dag = dag_from_json(&repeated_dag_text)
        .map_err(|e| format!("cannot parse repeated-plan DAG: {e}"))?;
    let before_display = dag_to_json(&repeated_dag);
    let display = exact_manifest_command_description(&repeated_dag, &repeated_metadata)?;
    let displayed_tag = format!("cell.{}", cell_run_slug(&repeated_metadata.cells[0], Some(1)));
    let displayed_node = repeated_dag.steps.iter().find(|step| step.tag() == displayed_tag)
        .ok_or("repeated command display lost its first node")?;
    if !display.contains(&format!("Node: {displayed_tag} (repetition 1 of 3)"))
        || !display.contains(&format!("Command:\n{}\n", displayed_node.cmd))
        || !display.contains(&format!("Wall bound: {}s", displayed_node.timeout))
        || !display.contains(&format!("CPU bound: {}s", effective_cpu_timeout(
            displayed_node, repeated_dag.default_step_cpu_timeout, repeated_dag.cpu_timeout_multiplier)))
        || displayed_node.env.iter().any(|(name, value)| !display.contains(&format!("{name}={}", shell_quote(value))))
        || dag_to_json(&repeated_dag) != before_display
    {
        return Err("exact command display changed the graph or misrepresented the selected repetition".into());
    }
    let mut missing_display = repeated_dag.clone();
    missing_display.steps.retain(|step| step.tag() != displayed_tag);
    let mut duplicate_display = repeated_dag.clone();
    duplicate_display.steps.push(displayed_node.clone());
    if exact_manifest_command_description(&missing_display, &repeated_metadata).is_ok()
        || exact_manifest_command_description(&duplicate_display, &repeated_metadata).is_ok()
    {
        return Err("exact command display accepted missing or ambiguous nodes".into());
    }
    let mut scaled_display_metadata = repeated_metadata.clone();
    scaled_display_metadata.timeout_policy = Some(PressureTimeoutPolicy {
        version: 1, cpu_multiplier: 2.0, wall_multiplier: 3.0,
    });
    let scaled_display = exact_manifest_command_description(&repeated_dag, &scaled_display_metadata)?;
    if !scaled_display.contains("HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=2\n")
        || !scaled_display.contains("HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=3\n")
        || dag_to_json(&repeated_dag) != before_display
    {
        return Err("exact command display conflated independent recorded multipliers or changed the graph".into());
    }
    let repeated_cell_steps: Vec<_> = repeated_dag
        .steps
        .iter()
        .filter(|step| step.group == "cell")
        .collect();
    let preparation_steps: Vec<_> = repeated_dag
        .steps
        .iter()
        .filter(|step| step.group == "prepare")
        .collect();
    let runtime_build_steps: Vec<_> = repeated_dag
        .steps
        .iter()
        .filter(|step| step.tag() == "build.runtime_release")
        .collect();
    let manifest_plan_steps: Vec<_> = repeated_dag
        .steps
        .iter()
        .filter(|step| step.tag() == "setup.manifest_plan")
        .collect();
    let recursive_metadata_tags = [
        "e2e.metadata",
        "build.workspace",
        "build.e2e_artifact",
    ];
    let repeated_jobs: BTreeSet<_> = repeated_cell_steps
        .iter()
        .map(|step| step.job.clone())
        .collect();
    let expected_repeated_jobs: BTreeSet<_> = (1..=3)
        .map(|number| cell_run_slug(&exact_id, Some(number)))
        .collect();
    if repeated_cell_steps.len() != 3
        || repeated_jobs != expected_repeated_jobs
        || preparation_steps.len() != 1
        || runtime_build_steps.len() != 1
        || manifest_plan_steps.len() != 1
        || repeated_dag
            .steps
            .iter()
            .any(|step| recursive_metadata_tags.contains(&step.tag().as_str()))
        || runtime_build_steps[0].deps
            != ["gate.manifest".to_string(), "pre.reverie_pin".to_string()]
        || manifest_plan_steps[0].deps != ["build.rust_scripts".to_string()]
        || !runtime_build_steps[0]
            .cmd
            .contains("cargo build --release --locked -p hermit --bin hermit")
        || !manifest_plan_steps[0]
            .cmd
            .contains("cargo build -p hermit-manifest-plan --bins")
        || manifest_plan_steps[0]
            .cmd
            .contains("cargo build --release --locked -p hermit --bin hermit")
        || repeated_dag.resource_caps.get("manifest_guest") != Some(&4)
        || repeated_dag.resource_caps.contains_key("kvm")
    {
        return Err(
            "repeated exact plan lost its shared direct build, preparation, cells, or resource caps, or reintroduced the recursive metadata audit"
                .into(),
        );
    }
    let preparation_tag = preparation_steps[0].tag();
    if preparation_steps[0].deps
        != ["setup.manifest_plan".to_string(), "build.runtime_release".to_string()]
    {
        return Err("repeated exact preparation does not depend on its direct Hermit build".into());
    }
    let repeated_tags: BTreeSet<_> = repeated_cell_steps.iter().map(|step| step.tag()).collect();
    for step in &repeated_cell_steps {
        if !step.deps.contains(&preparation_tag)
            || !step.deps.contains(&"setup.manifest_plan".to_string())
            || !step.deps.contains(&"build.runtime_release".to_string())
            || step.deps.iter().any(|dep| repeated_tags.contains(dep))
            || !step.cmd.contains("--prebuilt")
            || !step
                .cmd
                .contains("HERMIT_BIN=\"$PWD/target/release/hermit\"")
            || step.cmd.contains("run-with-hermit-e2e-artifact.sh")
            || !step
                .cmd
                .contains(&format!("E2E_RUN_ID='validate-one-pid100--{}'", step.job))
            || !step.cmd.contains(&format!(
                "{E2E_RUN_INDEX_ENV}={}",
                series_run_index(&step.job)
            ))
            || !step.cmd.contains(&format!("/cells/{}/", step.job))
        {
            return Err(format!(
                "{} does not share preparation while retaining a unique run ID and result path",
                step.tag()
            ));
        }
    }
    let summary_step = repeated_dag
        .steps
        .iter()
        .find(|step| step.tag() == "pressure.summarize")
        .ok_or("repeated plan lost pressure.summarize")?;
    if summary_step.deps.iter().cloned().collect::<BTreeSet<_>>() != repeated_tags {
        return Err("repeated summary does not depend on every repeated cell".into());
    }
    let repeated_timeouts: BTreeMap<_, _> = repeated_cell_steps
        .iter()
        .map(|step| (step.tag(), step.timeout))
        .collect();
    audit_dag(
        &repeated_dag,
        3,
        repeated_metadata.run_timeout_seconds,
        &repeated_timeouts,
    )?;
    let mut missing_repetition = repeated_dag.clone();
    let missing_job = repeated_cell_steps[0].job.clone();
    missing_repetition
        .steps
        .retain(|step| !(step.group == "cell" && step.job == missing_job));
    if audit_dag(
        &missing_repetition,
        3,
        repeated_metadata.run_timeout_seconds,
        &repeated_timeouts,
    )
    .is_ok()
    {
        return Err("repeated-plan audit accepted a missing cell job".into());
    }
    let mut missing_direct_build = repeated_dag.clone();
    missing_direct_build
        .steps
        .retain(|step| step.tag() != "build.runtime_release");
    if audit_dag(
        &missing_direct_build,
        3,
        repeated_metadata.run_timeout_seconds,
        &repeated_timeouts,
    )
    .is_ok()
    {
        return Err("repeated-plan audit accepted a missing required Hermit build".into());
    }
    let mut duplicate_repetition = repeated_dag.clone();
    let cell_indexes: Vec<_> = duplicate_repetition
        .steps
        .iter()
        .enumerate()
        .filter_map(|(index, step)| (step.group == "cell").then_some(index))
        .collect();
    duplicate_repetition.steps[cell_indexes[1]].job =
        duplicate_repetition.steps[cell_indexes[0]].job.clone();
    if audit_dag(
        &duplicate_repetition,
        3,
        repeated_metadata.run_timeout_seconds,
        &repeated_timeouts,
    )
    .is_ok()
    {
        return Err("repeated-plan audit accepted a duplicate cell job".into());
    }

    repeated_metadata.source_tree_dirty = false;
    validate_run_contract(root, &repeated_results, &repeated_metadata, false)
        .map_err(|e| format!("valid repeated run contract was refused: {e}"))?;
    for (label, mut invalid) in [
        ("zero repetitions", repeated_metadata.clone()),
        ("partial exact cell", repeated_metadata.clone()),
        ("sample", repeated_metadata.clone()),
    ] {
        match label {
            "zero repetitions" => invalid.repetitions = Some(0),
            "partial exact cell" => invalid.mode = None,
            "sample" => invalid.sample = Some(1),
            _ => unreachable!(),
        }
        if validate_run_contract(root, &repeated_results, &invalid, false).is_ok() {
            return Err(format!("retained repeated run accepted {label}"));
        }
    }
    let mut dirty_repeated_metadata = repeated_metadata.clone();
    dirty_repeated_metadata.source_tree_dirty = true;
    if validate_run_contract(root, &repeated_results, &dirty_repeated_metadata, true).is_ok() {
        return Err("dirty repeated run metadata was accepted".into());
    }
    let mut impossible_population_metadata = repeated_metadata.clone();
    impossible_population_metadata.green = true;
    impossible_population_metadata.probe_disabled = true;
    let impossible_population_error = validate_run_contract(
        root,
        &repeated_results,
        &impossible_population_metadata,
        false,
    )
    .err()
    .ok_or("retained run accepted mutually exclusive green and disabled populations")?;
    if !impossible_population_error.contains("--probe-disabled and --green are mutually exclusive")
    {
        return Err(format!(
            "retained green-plus-disabled run reported the wrong error: {impossible_population_error}"
        ));
    }
    let mut unscoped_disabled_metadata = repeated_metadata.clone();
    unscoped_disabled_metadata.probe_disabled = true;
    unscoped_disabled_metadata.backend = None;
    let unscoped_disabled_error = validate_run_contract(
        root,
        &repeated_results,
        &unscoped_disabled_metadata,
        false,
    )
    .err()
    .ok_or("retained run accepted an unscoped disabled population")?;
    if !unscoped_disabled_error.contains("--probe-disabled requires --backend") {
        return Err(format!(
            "retained unscoped disabled run reported the wrong error: {unscoped_disabled_error}"
        ));
    }

    let repeated_build_results = scratch.join("repeated-build-markers");
    let setup_marker = build_marker(&repeated_build_results, "setup.manifest_plan");
    let runtime_marker = build_marker(&repeated_build_results, "build.runtime_release");
    fs::create_dir_all(runtime_marker.parent().expect("build marker has parent"))
        .map_err(|e| format!("cannot create repeated build-marker fixture: {e}"))?;
    if required_builds_complete(&repeated_build_results, &repeated_metadata) {
        return Err("repeated exact ptrace setup accepted a missing Hermit build".into());
    }
    fs::write(&runtime_marker, "ok\n")
        .map_err(|e| format!("cannot write repeated runtime marker: {e}"))?;
    if required_builds_complete(&repeated_build_results, &repeated_metadata) {
        return Err("repeated exact ptrace setup accepted a missing Rust runner build".into());
    }
    fs::write(&setup_marker, "ok\n")
        .map_err(|e| format!("cannot write repeated runner marker: {e}"))?;
    for tag in ["pre.submodules", "pre.reverie_pin", "build.rust_scripts", "gate.manifest"] {
        if required_builds_complete(&repeated_build_results, &repeated_metadata) {
            return Err(format!("repeated exact setup accepted missing prerequisite marker {tag}"));
        }
        fs::write(build_marker(&repeated_build_results, tag), "ok\n")
            .map_err(|e| format!("cannot write prerequisite marker {tag}: {e}"))?;
    }
    if !required_builds_complete(&repeated_build_results, &repeated_metadata) {
        return Err("repeated exact ptrace setup refused its direct Hermit build".into());
    }
    for tag in ["pre.submodules", "pre.reverie_pin", "build.rust_scripts", "gate.manifest"] {
        let marker = build_marker(&repeated_build_results, tag);
        fs::remove_file(&marker)
            .map_err(|e| format!("cannot remove prerequisite marker {tag}: {e}"))?;
        if required_builds_complete(&repeated_build_results, &repeated_metadata) {
            return Err(format!("otherwise complete setup accepted missing prerequisite marker {tag}"));
        }
        fs::write(&marker, "ok\n")
            .map_err(|e| format!("cannot restore prerequisite marker {tag}: {e}"))?;
        if !required_builds_complete(&repeated_build_results, &repeated_metadata) {
            return Err(format!("restoring prerequisite marker {tag} did not restore completed setup"));
        }
    }

    let red_batch_selection = CellSelection {
        repetitions: Some(2),
        run_timeout_seconds: Some(1_000_000),
        ..CellSelection::default()
    };
    let selected_red_batch = pressure_cells(root, &red_batch_selection)?;
    let selected_red_ids: BTreeSet<_> = selected_red_batch
        .selected
        .iter()
        .map(|tracked| tracked.id.clone())
        .collect();
    let unavailable_red_ids: BTreeSet<_> = selected_red_batch
        .unavailable
        .iter()
        .map(|tracked| tracked.id.clone())
        .collect();
    if selected_red_ids != expected_red_ids
        || unavailable_red_ids != expected_unavailable_red_ids
        || selected_red_batch.eligible_cells != expected_red_ids.len()
    {
        return Err("repeated red batch did not retain the complete red population".into());
    }

    let green_batch_selection = CellSelection {
        green: true,
        repetitions: Some(2),
        run_timeout_seconds: Some(1_000_000),
        ..CellSelection::default()
    };
    let expected_green_ids: BTreeSet<_> = tracked
        .cells
        .iter()
        .filter(|tracked| tracked.enabled && tracked.status == "green")
        .map(|tracked| tracked.id.clone())
        .collect();
    let selected_green_batch = pressure_cells(root, &green_batch_selection)?;
    let selected_green_ids: BTreeSet<_> = selected_green_batch
        .selected
        .iter()
        .map(|tracked| tracked.id.clone())
        .collect();
    if selected_green_ids != expected_green_ids || !selected_green_batch.unavailable.is_empty() {
        return Err("--green did not select the complete enabled green population".into());
    }
    let green_sample_selection = CellSelection {
        green: true,
        repetitions: Some(1),
        sample: Some(2),
        seed: Some(7),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let green_sample = pressure_cells(root, &green_sample_selection)?;
    if green_sample.selected.len() != 2 || green_sample.eligible_cells != expected_green_ids.len() {
        return Err(
            "seeded repeated-green sampling lost its selected or eligible-cell count".into(),
        );
    }
    let one_cell_mode_results = scratch.join("one-cell-mode-green-plan");
    let one_cell_mode_selection = CellSelection {
        green: true,
        mode: Some("replay".into()),
        repetitions: Some(2),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let (one_cell_mode_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &one_cell_mode_results,
        &one_cell_mode_results.join("dag.json"),
        &one_cell_mode_selection,
    )?;
    if !one_cell_mode_metadata.green
        || one_cell_mode_metadata.cells.len() != 1
        || top_level_repeated_result_description(&one_cell_mode_metadata, 1, 1, 0, 0, 2)
            != "one or more repeated checks failed or required a retry"
    {
        return Err(
            "a one-cell mode-filtered green batch was described as an exact flaky cell".into(),
        );
    }
    let one_cell_sample_results = scratch.join("one-cell-sample-green-plan");
    let one_cell_sample_selection = CellSelection {
        green: true,
        repetitions: Some(2),
        sample: Some(1),
        seed: Some(7),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let (one_cell_sample_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &one_cell_sample_results,
        &one_cell_sample_results.join("dag.json"),
        &one_cell_sample_selection,
    )?;
    if !one_cell_sample_metadata.green
        || one_cell_sample_metadata.cells.len() != 1
        || top_level_repeated_result_description(&one_cell_sample_metadata, 1, 1, 0, 0, 2)
            != "one or more repeated checks failed or required a retry"
    {
        return Err("a one-cell sampled green batch was described as an exact flaky cell".into());
    }
    let green_batch_results = scratch.join("green-batch-plan");
    let (mut green_batch_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &green_batch_results,
        &green_batch_results.join("dag.json"),
        &green_batch_selection,
    )?;
    if !green_batch_metadata.green
        || green_batch_metadata.repetitions != Some(2)
        || green_batch_metadata.eligible_cells != expected_green_ids.len()
        || green_batch_metadata
            .cells
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_green_ids
    {
        return Err("green batch metadata did not bind the complete selected population".into());
    }
    let mut red_batch_result_metadata = green_batch_metadata.clone();
    red_batch_result_metadata.green = false;
    let disabled_batch_result_metadata = disabled_batch_metadata;
    if !repeated_metadata.is_exact()
        || green_batch_metadata.is_exact()
        || top_level_repeated_result_description(&repeated_metadata, 1, 1, 0, 0, 2)
            != "flaky"
        || top_level_repeated_result_description(&red_batch_result_metadata, 1, 1, 0, 0, 2)
            != "one or more repeated checks failed or required a retry"
    {
        return Err(
            "repeated exact and batch results were classified by color instead of shape".into(),
        );
    }
    let exact_red_heading = summary_heading(&repeated_metadata);
    let exact_red_result = repeated_summary_line(&repeated_metadata, 1, 1, 0, 0, 2);
    let retried_exact_red_result = repeated_summary_line(&repeated_metadata, 2, 1, 0, 1, 2);
    let all_recovered_exact_red_result =
        repeated_summary_line(&repeated_metadata, 2, 0, 0, 2, 2);
    let all_failed_exact_red_result =
        repeated_summary_line(&repeated_metadata, 0, 0, 0, 2, 2);
    let red_batch_heading = summary_heading(&red_batch_result_metadata);
    let red_batch_result =
        repeated_summary_line(&red_batch_result_metadata, 1, 1, 0, 0, 2);
    let one_recovered_red_batch_result =
        repeated_summary_line(&red_batch_result_metadata, 2, 1, 0, 1, 2);
    let recovered_red_batch_result =
        repeated_summary_line(&red_batch_result_metadata, 2, 0, 0, 2, 2);
    let failed_red_batch_result =
        repeated_summary_line(&red_batch_result_metadata, 0, 0, 0, 2, 2);
    let green_batch_heading = summary_heading(&green_batch_metadata);
    let green_batch_result = repeated_summary_line(&green_batch_metadata, 1, 1, 0, 0, 2);
    let disabled_batch_heading = summary_heading(&disabled_batch_result_metadata);
    let disabled_batch_result =
        repeated_summary_line(&disabled_batch_result_metadata, 1, 1, 0, 0, 2);
    if exact_red_heading != "# Repeated red-cell results"
        || exact_red_result
            != "Repeated result: 1/2 terminally passed; 1/2 passed cleanly; flaky."
        || retried_exact_red_result
            != "Repeated result: 2/2 terminally passed; 1/2 passed cleanly; flaky."
        || all_recovered_exact_red_result
            != "Repeated result: 2/2 terminally passed; 0/2 passed cleanly; flaky."
        || all_failed_exact_red_result
            != "Repeated result: 0/2 terminally passed; 0/2 passed cleanly; failed every repetition."
        || red_batch_heading != "# Repeated red-cell results"
        || red_batch_result
            != "Repeated red-cell batch: 1/2 terminally passed; 1/2 passed cleanly; one or more repeated checks failed or required a retry."
        || one_recovered_red_batch_result
            != "Repeated red-cell batch: 2/2 terminally passed; 1/2 passed cleanly; one or more repeated checks failed or required a retry."
        || recovered_red_batch_result
            != "Repeated red-cell batch: 2/2 terminally passed; 0/2 passed cleanly; one or more repeated checks failed or required a retry."
        || failed_red_batch_result
            != "Repeated red-cell batch: 0/2 terminally passed; 0/2 passed cleanly; one or more repeated checks failed or required a retry."
        || green_batch_heading != "# Repeated green-cell results"
        || green_batch_result
            != "Repeated green-cell batch: 1/2 terminally passed; 1/2 passed cleanly; one or more repeated checks failed or required a retry."
        || disabled_batch_heading != "# Repeated disabled-cell results"
        || disabled_batch_result
            != "Repeated disabled-cell batch: 1/2 terminally passed; 1/2 passed cleanly; one or more repeated checks failed or required a retry."
    {
        return Err(format!(
            "repeated summary rendering mislabeled an exact red, red batch, green batch, or disabled batch: \
             exact={exact_red_heading:?}/{exact_red_result:?} \
             red_batch={red_batch_heading:?}/{red_batch_result:?} \
             green_batch={green_batch_heading:?}/{green_batch_result:?} \
             disabled_batch={disabled_batch_heading:?}/{disabled_batch_result:?}"
        ));
    }
    let green_batch_dag_text = fs::read_to_string(green_batch_results.join("dag.json"))
        .map_err(|e| format!("cannot read green-batch DAG: {e}"))?;
    let green_batch_dag = dag_from_json(&green_batch_dag_text)
        .map_err(|e| format!("cannot parse green-batch DAG: {e}"))?;
    let green_batch_includes_liteinst = expected_green_ids
        .iter()
        .any(|cell| cell.backend == "liteinst");
    let expected_green_build_tags: BTreeSet<String> =
        required_build_tags(None, green_batch_includes_liteinst)
            .into_iter()
            .map(str::to_string)
            .collect();
    let actual_green_build_tags: BTreeSet<String> = green_batch_dag
        .steps
        .iter()
        .filter(|step| matches!(step.group.as_str(), "pre" | "gate" | "build" | "setup"))
        .map(|step| step.tag())
        .collect();
    if actual_green_build_tags != expected_green_build_tags
        || green_batch_dag
            .steps
            .iter()
            .any(|step| step.tag() == "e2e.metadata")
    {
        return Err(format!(
            "green batch build set changed or reintroduced the recursive metadata audit: expected={expected_green_build_tags:?} actual={actual_green_build_tags:?}"
        ));
    }
    for step in green_batch_dag
        .steps
        .iter()
        .filter(|step| matches!(step.group.as_str(), "pre" | "gate" | "build" | "setup"))
    {
        let deps: BTreeSet<&str> = step.deps.iter().map(String::as_str).collect();
        let tag = step.tag();
        let expected: BTreeSet<&str> = match tag.as_str() {
            "pre.submodules" => BTreeSet::new(),
            "pre.reverie_pin" => BTreeSet::from(["pre.submodules"]),
            "build.rust_scripts" => BTreeSet::from(["pre.reverie_pin"]),
            "setup.manifest_plan" => BTreeSet::from(["build.rust_scripts"]),
            "gate.manifest" => BTreeSet::from(["setup.manifest_plan"]),
            "setup.nextest" => BTreeSet::from(["build.rust_scripts", "gate.manifest", "pre.reverie_pin"]),
            "build.workspace" => BTreeSet::from(["gate.manifest", "pre.reverie_pin", "setup.nextest"]),
            "build.runtime_release" => BTreeSet::from(["gate.manifest", "pre.reverie_pin"]),
            "build.e2e_artifact" => BTreeSet::from([
                "build.workspace", "build.runtime_release", "gate.manifest", "pre.reverie_pin",
            ]),
            "build.liteinst_runtime_release" => BTreeSet::from([
                "build.e2e_artifact", "gate.manifest", "pre.reverie_pin",
            ]),
            other => return Err(format!("unexpected green-batch build node {other}")),
        };
        if deps != expected {
            return Err(format!(
                "{} lost its internal build dependencies: expected={expected:?} actual={deps:?}",
                tag
            ));
        }
        if tag == "build.e2e_artifact"
            && !step.cmd.contains("./ci/publish-hermit-e2e-artifact.sh")
        {
            return Err("green batch replaced the canonical prebuilt artifact publisher".into());
        }
    }
    let batch_build_results = scratch.join("batch-nextest-build-markers");
    fs::create_dir_all(batch_build_results.join("state"))
        .map_err(|e| format!("cannot create batch build marker fixture: {e}"))?;
    for tag in &expected_green_build_tags {
        fs::write(build_marker(&batch_build_results, tag), "ok\n")
            .map_err(|e| format!("cannot write batch marker {tag}: {e}"))?;
    }
    if !required_builds_complete(&batch_build_results, &green_batch_metadata) {
        return Err("batch setup refused its complete prerequisite markers".into());
    }
    let nextest_marker = build_marker(&batch_build_results, "setup.nextest");
    fs::remove_file(&nextest_marker)
        .map_err(|e| format!("cannot remove Nextest marker: {e}"))?;
    if required_builds_complete(&batch_build_results, &green_batch_metadata) {
        return Err("otherwise complete batch setup accepted missing Nextest preparation".into());
    }
    fs::write(&nextest_marker, "ok\n")
        .map_err(|e| format!("cannot restore Nextest marker: {e}"))?;
    if !required_builds_complete(&batch_build_results, &green_batch_metadata) {
        return Err("restoring the Nextest marker did not restore completed batch setup".into());
    }
    let green_batch_cell_count = green_batch_dag
        .steps
        .iter()
        .filter(|step| step.group == "cell")
        .count();
    let green_batch_preparation_count = green_batch_dag
        .steps
        .iter()
        .filter(|step| step.group == "prepare")
        .count();
    let green_test_count = expected_green_ids
        .iter()
        .map(|cell| cell.test.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let expected_green_cell_runs = expected_green_ids.len() * green_batch_selection.run_count();
    if green_batch_cell_count != expected_green_cell_runs
        || green_batch_preparation_count != green_test_count
        || green_batch_dag
            .steps
            .iter()
            .filter(|step| step.group == "prepare")
            .any(|step| {
                step.deps
                    != ["setup.manifest_plan".to_string(), "build.e2e_artifact".to_string()]
            })
        || green_batch_dag
            .steps
            .iter()
            .filter(|step| step.group == "cell")
            .any(|step| {
                !step.deps.contains(&"build.e2e_artifact".to_string())
                    || !step.deps.contains(&"setup.manifest_plan".to_string())
                    || !step.cmd.contains(
                        "./ci/run-with-hermit-e2e-artifact.sh --require-install",
                    )
            })
    {
        return Err(
            "green batch did not retain every selected cell, shared preparation, and canonical prebuilt artifact in one DAG"
                .into(),
        );
    }
    let mut missing_green_artifact = green_batch_dag.clone();
    missing_green_artifact
        .steps
        .retain(|step| step.tag() != "build.e2e_artifact");
    let green_batch_timeouts: BTreeMap<_, _> = green_batch_dag
        .steps
        .iter()
        .filter(|step| step.group == "cell")
        .map(|step| (step.tag(), step.timeout))
        .collect();
    if audit_dag(
        &missing_green_artifact,
        expected_green_cell_runs,
        green_batch_metadata.run_timeout_seconds,
        &green_batch_timeouts,
    )
    .is_ok()
    {
        return Err("green-batch plan audit accepted a missing prebuilt artifact".into());
    }
    green_batch_metadata.source_tree_dirty = false;
    validate_run_contract(root, &green_batch_results, &green_batch_metadata, false)
        .map_err(|e| format!("complete green batch contract was refused: {e}"))?;
    let mut incomplete_green_batch = green_batch_metadata.clone();
    incomplete_green_batch.cells.pop();
    if validate_run_contract(root, &green_batch_results, &incomplete_green_batch, false).is_ok() {
        return Err("green batch contract accepted an incomplete selected population".into());
    }
    let mut forged_green_denominator = green_batch_metadata.clone();
    forged_green_denominator.eligible_cells += 1;
    if validate_run_contract(root, &green_batch_results, &forged_green_denominator, false).is_ok() {
        return Err("green batch contract accepted a forged eligible-cell count".into());
    }
    let forged_zero_results = scratch.join("forged-zero-plan");
    fs::create_dir_all(&forged_zero_results)
        .map_err(|e| format!("cannot create forged-zero fixture: {e}"))?;
    let mut forged_zero_dag = green_batch_dag.clone();
    forged_zero_dag.steps.retain(|step| step.group != "cell");
    if let Some(summary) = forged_zero_dag
        .steps
        .iter_mut()
        .find(|step| step.tag() == "pressure.summarize")
    {
        summary.deps.clear();
    }
    fs::write(
        forged_zero_results.join("dag.json"),
        format!("{}\n", dag_to_json(&forged_zero_dag)),
    )
    .map_err(|e| format!("cannot write forged-zero DAG: {e}"))?;
    for tag in required_build_tags(None, false) {
        let marker = build_marker(&forged_zero_results, tag);
        fs::create_dir_all(marker.parent().expect("build marker has parent"))
            .map_err(|e| format!("cannot create forged-zero build marker: {e}"))?;
        fs::write(marker, "ok\n")
            .map_err(|e| format!("cannot write forged-zero build marker: {e}"))?;
    }
    let mut forged_zero_metadata = green_batch_metadata.clone();
    forged_zero_metadata.sample = Some(0);
    forged_zero_metadata.seed = Some(7);
    forged_zero_metadata.cells.clear();
    if validate_run_contract(root, &forged_zero_results, &forged_zero_metadata, false).is_ok() {
        return Err("repeated green batch accepted forged 0/0 evidence".into());
    }
    forged_zero_metadata.sample = None;
    forged_zero_metadata.seed = None;
    if validate_run_contract(root, &forged_zero_results, &forged_zero_metadata, false).is_ok() {
        return Err("unqualified repeated green batch accepted an empty population".into());
    }

    let sample_selection = CellSelection {
        sample: Some(2),
        seed: Some(7),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let sample_results = scratch.join("sample-plan");
    let (mut sample_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &sample_results,
        &sample_results.join("dag.json"),
        &sample_selection,
    )?;
    if sample_metadata.cells.len() != 2 {
        return Err("generated sampled plan did not retain its requested two cells".into());
    }
    sample_metadata.source_tree_dirty = false;
    validate_run_contract(root, &sample_results, &sample_metadata, true)
        .map_err(|e| format!("clean retained batch could not be re-summarized: {e}"))?;
    sample_metadata.source_tree_dirty = true;
    if validate_run_contract(root, &sample_results, &sample_metadata, true).is_ok() {
        return Err("dirty non-exact retained batch was accepted".into());
    }
    sample_metadata.source_tree_dirty = false;

    let profile_dir = scratch.join("runner-profile");
    fs::create_dir(&profile_dir)
        .map_err(|e| format!("cannot create self-test profile directory: {e}"))?;
    fs::write(
        profile_dir.join("step_profiles_fixture.csv"),
        "git_sha,step,ok,timed_out,cpu_timed_out,oom_kills\n\
         abc,cell.pass,true,false,false,0\n\
         abc,cell.oom,false,false,false,2\n\
         abc,cell.timeout,false,true,false,0\n\
         abc,cell.runner-failed,false,false,false,0\n\
         abc,\"cell,quoted\",true,false,false,0\n\
         other,cell.foreign,true,false,false,0\n\
         other,cell.foreign-oom,false,false,false,9\n",
    )
    .map_err(|e| format!("cannot write self-test runner profile: {e}"))?;
    let retained = load_runner_evidence(&scratch, "abc")?;
    if !retained
        .get("cell.pass")
        .is_some_and(|row| row.seen && row.ok)
        || !retained.get("cell.oom").is_some_and(|row| row.oom)
        || !retained
            .get("cell.timeout")
            .is_some_and(|row| row.timed_out)
        || !retained
            .get("cell.runner-failed")
            .is_some_and(|row| row.seen && !row.ok && !row.oom && !row.timed_out)
        || !retained
            .get("cell,quoted")
            .is_some_and(|row| row.seen && row.ok)
        || retained.contains_key("cell.foreign")
        || retained.contains_key("cell.foreign-oom")
    {
        return Err("retained runner evidence did not preserve pass/OOM/timeout identity".into());
    }
    if !reason_reports_timeout(Some(
        "test fixture/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: deadline reached (exit 124))",
    )) || !reason_reports_timeout(Some(
        "test fixture/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: SIGKILL after 10 s grace (exit 137))",
    )) || reason_reports_timeout(Some("deadline reached (exit 124)"))
        || reason_reports_timeout(Some("guest timed out"))
        || reason_reports_timeout(Some("verify exited with status 1"))
    {
        return Err("timeout failure bucketing lost its positive or negative bracket".into());
    }
    let runner_ok = RunnerEvidence {
        seen: true,
        ok: true,
        timed_out: false,
        oom: false,
        output_log_available: true,
        environmental_block_observation: EnvBlockObservation::NoDenial,
    };
    let runner_oom = RunnerEvidence {
        ok: false,
        oom: true,
        ..runner_ok
    };
    let runner_oom_pass = RunnerEvidence {
        ok: true,
        oom: true,
        ..runner_ok
    };
    let runner_timeout = RunnerEvidence {
        ok: false,
        timed_out: true,
        ..runner_ok
    };
    let runner_timeout_pass = RunnerEvidence {
        ok: true,
        timed_out: true,
        ..runner_ok
    };
    let runner_failed = RunnerEvidence {
        ok: false,
        ..runner_ok
    };
    let runner_sandbox_denied = RunnerEvidence {
        ok: false,
        environmental_block_observation: EnvBlockObservation::Denied(
            EnvBlockClass::BpfjailerBanner,
        ),
        ..runner_ok
    };
    let runner_proxy_denied = RunnerEvidence {
        ok: false,
        environmental_block_observation: EnvBlockObservation::Denied(EnvBlockClass::ProxyEgress),
        ..runner_ok
    };
    let first_repetition_tag = format!("cell.{}", cell_run_slug(&green_id, Some(1)));
    let second_repetition_tag = format!("cell.{}", cell_run_slug(&green_id, Some(2)));
    let timeout_for_first = BTreeMap::from([(first_repetition_tag.clone(), runner_timeout)]);
    if is_proven_timeout_attempt(
        timeout_for_first
            .get(&second_repetition_tag)
            .copied()
            .unwrap_or_default(),
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("timeout evidence crossed between repeated cell jobs".into());
    }
    let oom_for_first = BTreeMap::from([(first_repetition_tag, runner_oom)]);
    if is_proven_oom_attempt(
        oom_for_first
            .get(&second_repetition_tag)
            .copied()
            .unwrap_or_default(),
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("OOM evidence crossed between repeated cell jobs".into());
    }
    if !is_proven_oom_attempt(runner_oom, Some(INCOMPLETE_ATTEMPT_STATUS))
        || !is_proven_oom_attempt(runner_oom, Some(137))
        || is_proven_oom_attempt(runner_oom, None)
        || is_proven_oom_attempt(runner_oom, Some(0))
        || is_proven_oom_attempt(runner_oom, Some(PREPARATION_FAILED_STATUS))
        || is_proven_oom_attempt(runner_oom_pass, Some(INCOMPLETE_ATTEMPT_STATUS))
        || is_proven_oom_attempt(runner_ok, Some(INCOMPLETE_ATTEMPT_STATUS))
    {
        return Err(
            "OOM proof did not require a failed exact runner OOM row and a non-pass, non-preparation harness marker"
                .into(),
        );
    }
    if !is_proven_timeout_attempt(runner_timeout, Some(INCOMPLETE_ATTEMPT_STATUS))
        || is_proven_timeout_attempt(runner_timeout, Some(124))
        || is_proven_timeout_attempt(runner_timeout, None)
        || is_proven_timeout_attempt(runner_timeout_pass, Some(INCOMPLETE_ATTEMPT_STATUS))
        || is_proven_timeout_attempt(runner_ok, Some(INCOMPLETE_ATTEMPT_STATUS))
    {
        return Err(
            "timeout proof did not require both a failed exact runner timeout row and the incomplete-attempt marker"
                .into(),
        );
    }
    if !runner_observed_terminal_attempt(runner_ok, Some(0))
        || !runner_observed_terminal_attempt(runner_failed, Some(1))
        || runner_observed_terminal_attempt(runner_ok, Some(1))
        || runner_observed_terminal_attempt(runner_failed, Some(0))
        || runner_observed_terminal_attempt(runner_ok, Some(INCOMPLETE_ATTEMPT_STATUS))
        || runner_observed_terminal_attempt(runner_ok, Some(PREPARATION_FAILED_STATUS))
        || runner_observed_terminal_attempt(runner_timeout, Some(1))
    {
        return Err(
            "terminal runner evidence lost pass/failure agreement or accepted an incomplete, preparation-failed, or runner-killed attempt"
                .into(),
        );
    }
    let classifications = [
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("matched"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(1),
            "FAIL",
            true,
            None,
            "verify",
            Some("diverged"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(1),
            "FAIL",
            true,
            None,
            "replay",
            Some("diverged"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(1),
            "FAIL",
            true,
            None,
            "verify",
            Some("no_result"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(122),
            "ERROR",
            true,
            Some("verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)"),
            "verify",
            Some("infrastructure_error"),
            true,
            true,
        ),
        classify_result(
            runner_timeout,
            Some(INCOMPLETE_ATTEMPT_STATUS),
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            true,
            true,
        ),
        classify_result(
            runner_timeout,
            Some(INCOMPLETE_ATTEMPT_STATUS),
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            false,
            false,
        ),
        classify_result(
            runner_failed,
            Some(1),
            "FAIL",
            true,
            Some(
                "test fixture/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: deadline reached (exit 124))",
            ),
            "verify",
            // A run killed at its inner deadline never reached comparison, so
            // it has no verdict, retained no verification logs, and carries no
            // valid verification evidence. Asserting otherwise made this case
            // agree under either branch order and so tested nothing.
            None,
            false,
            false,
        ),
        classify_result(
            runner_failed,
            Some(124),
            "FAIL",
            true,
            None,
            "naked",
            None,
            false,
            true,
        ),
        classify_result(
            runner_oom,
            Some(137),
            "FAIL",
            false,
            None,
            "verify",
            None,
            true,
            true,
        ),
        classify_result(
            runner_oom,
            None,
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            false,
            true,
        ),
        classify_result(
            runner_oom,
            Some(INCOMPLETE_ATTEMPT_STATUS),
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            false,
            false,
        ),
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("no_result"),
            true,
            true,
        ),
        classify_result(
            RunnerEvidence::default(),
            Some(124),
            "FAIL",
            false,
            Some("timed out"),
            "verify",
            None,
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("matched"),
            false,
            true,
        ),
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("matched"),
            true,
            false,
        ),
        classify_result(
            runner_sandbox_denied,
            Some(1),
            "FAIL",
            true,
            Some("1 failed"),
            "verify",
            Some("no_result"),
            false,
            false,
        ),
        classify_result(
            runner_proxy_denied,
            Some(1),
            "FAIL",
            true,
            Some("could not resolve proxy"),
            "verify",
            Some("no_result"),
            false,
            false,
        ),
    ];
    if classifications
        != [
            "pass",
            "determinism-failure",
            "replay-failure",
            "crash-error",
            "infrastructure-error",
            "timeout",
            "infrastructure-error",
            "timeout",
            "crash-error",
            "oom",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "sandbox-denied",
            "infrastructure-error",
        ]
    {
        return Err(format!(
            "failure bucketing changed unexpectedly: {classifications:?}"
        ));
    }
    if repeated_result_description(2, 2, 0, 0, 2) != "passed every repetition"
        || repeated_result_description(2, 1, 0, 1, 2) != "flaky"
        || repeated_result_description(1, 0, 0, 1, 1) != "flaky"
        || repeated_result_description(2, 0, 0, 2, 2) != "flaky"
        || repeated_result_description(1, 1, 0, 0, 2) != "flaky"
        || repeated_result_description(0, 0, 0, 0, 2) != "failed every repetition"
        || repeated_result_description(0, 0, 0, 2, 2) != "failed every repetition"
        || repeated_result_description(1, 1, 1, 0, 2) != "incomplete"
        || repeated_result_description(0, 0, 2, 0, 2) != "incomplete"
        || repeated_batch_result_description(2, 1, 0, 1, 2)
            != "one or more repeated checks failed or required a retry"
        || repeated_batch_result_description(2, 0, 0, 2, 2)
            != "one or more repeated checks failed or required a retry"
        || repeated_batch_result_description(0, 0, 0, 2, 2)
            != "one or more repeated checks failed or required a retry"
        || repeated_batch_result_description(1, 1, 0, 0, 2)
            != "one or more repeated checks failed or required a retry"
        || repeated_batch_result_description(1, 1, 1, 0, 2) != "incomplete"
        || repeated_run_has_unacceptable_product_result(Some(2), true, 1, 0, 2)
        || repeated_run_has_unacceptable_product_result(Some(2), true, 2, 1, 2)
        || !repeated_run_has_unacceptable_product_result(Some(2), false, 1, 0, 2)
        || !repeated_run_has_unacceptable_product_result(Some(2), false, 2, 1, 2)
        || !repeated_run_has_unacceptable_product_result(Some(2), false, 0, 0, 0)
        || repeated_run_has_unacceptable_product_result(None, false, 0, 0, 1)
    {
        return Err(
            "repeated result confused missing evidence with trustworthy pass/failure outcomes"
                .into(),
        );
    }
    pressure_sample_classification_self_test()?;
    let sample_a = CellId {
        lane: "portable".into(),
        category: "sample".into(),
        test: "sample/a".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let sample_b = CellId {
        test: "sample/b".into(),
        ..sample_a.clone()
    };
    if sample_score(&sample_a, 42) == sample_score(&sample_b, 42)
        || sample_score(&sample_a, 42) == sample_score(&sample_a, 43)
    {
        return Err("seeded cell sampling lost its identity or seed sensitivity".into());
    }

    let sample_slug = base_cell_slug(&sample_a);
    let sample_metadata = RunMetadata {
        schema: RUN_SCHEMA,
        run_id: "sample-run".into(),
        hermit_sha: "abc".into(),
        detcore_tree: "def".into(),
        source_tree_dirty: false,
        run_timeout_seconds: 60,
        timeout_policy: None,
        mode: Some(sample_a.mode.clone()),
        test: Some(sample_a.test.clone()),
        backend: Some(sample_a.backend.clone()),
        cell_timeout_seconds: Some(20),
        sample: None,
        seed: None,
        unavailable_cells: 0,
        repetitions: None,
        run_id_prefix: None,
        green: false,
        probe_disabled: false,
        jobs: default_jobs(),
        manifest_guest_cap: DEFAULT_MANIFEST_GUEST_CAP,
        manifest_guest_cap_explicit: false,
        kvm_guest_cap: DEFAULT_KVM_GUEST_CAP,
        kvm_guest_cap_explicit: false,
        manifest_guest_memory_budget_bytes: None,
        manifest_guest_memory_required_bytes: None,
        manifest_guest_control_plane_headroom_bytes: None,
        manifest_guest_max_safe_cap: None,
        kvm_guest_max_safe_cap: None,
        eligible_cells: 1,
        cells_file: None,
        cells_file_sha256: None,
        selected_population_sha256: None,
        cells: vec![sample_a.clone()],
    };
    if current_result_policy(&sample_metadata, false)?
        || current_result_policy(&sample_metadata, true).is_ok()
    {
        return Err("fresh admission and retained historical metadata were conflated".into());
    }
    let mut current_metadata = sample_metadata.clone();
    current_metadata.timeout_policy = Some(PressureTimeoutPolicy {
        version: 1, cpu_multiplier: 1.5, wall_multiplier: 2.0,
    });
    if !current_result_policy(&current_metadata, false)? || !current_result_policy(&current_metadata, true)? {
        return Err("current pressure metadata lost strict admission".into());
    }
    let mut current_json = serde_json::to_value(&current_metadata).map_err(|error| error.to_string())?;
    let restored: RunMetadata = serde_json::from_value(current_json.clone()).map_err(|error| error.to_string())?;
    let restored_policy = restored.timeout_policy.ok_or("timeout policy vanished in round trip")?;
    if restored_policy.cpu_multiplier != 1.5 || restored_policy.wall_multiplier != 2.0 {
        return Err("retained CPU and wall multipliers were conflated".into());
    }
    for malformed in [
        JsonValue::Null,
        json!({"version": 2, "cpu_multiplier": 1.0, "wall_multiplier": 1.0}),
        json!({"version": 1, "cpu_multiplier": 1.0}),
        json!({"version": 1, "cpu_multiplier": 0.0, "wall_multiplier": 1.0}),
        json!({"version": 1, "cpu_multiplier": "1", "wall_multiplier": 1.0}),
        json!({"version": 1, "cpu_multiplier": 1.0, "wall_multiplier": 1.0, "unknown": true}),
    ] {
        current_json["timeout_policy"] = malformed;
        if serde_json::from_value::<RunMetadata>(current_json.clone()).is_ok() {
            return Err("malformed timeout policy fell back to historical interpretation".into());
        }
    }
    if retained_attempt_count(
        &[],
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        runner_ok,
        Some(0),
    )? != 1
        || retained_attempt_count(
            &[],
            &sample_slug,
            &sample_metadata,
            &sample_a,
            true,
            runner_timeout,
            Some(INCOMPLETE_ATTEMPT_STATUS),
        )? != 1
        || retained_attempt_count(
            &[],
            &sample_slug,
            &sample_metadata,
            &sample_a,
            true,
            runner_ok,
            Some(PREPARATION_FAILED_STATUS),
        )? != 0
        || retained_attempt_count(
            &[],
            &sample_slug,
            &sample_metadata,
            &sample_a,
            true,
            runner_ok,
            None,
        )? != 0
    {
        return Err(
            "attempt counting did not distinguish a begun harness attempt from a cell that never ran"
                .into(),
        );
    }
    let sample_artifact_dir = scratch
        .join("runs")
        .join(&sample_slug)
        .join("sample-a-verify-ptrace");
    let mut result_row = CellResult {
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        attempt: 1,
        schema: CELL_RESULT_SCHEMA,
        run_id: sample_slug.clone(),
        run_index: Some(0),
        machine_shortname: "fixture-host".into(),
        kernel_version: "7.1.3-fixture".into(),
        host_capabilities: fixture_host_capabilities(),
        hermit_sha: sample_metadata.hermit_sha.clone(),
        source_tree_dirty: false,
        binary_sha256: None,
        binary_build_sha: None,
        test_sha256: "fixture-test-sha256".into(),
        test: sample_a.test.clone(),
        category: sample_a.category.clone(),
        lane: sample_a.lane.clone(),
        mode: sample_a.mode.clone(),
        backend: Some(sample_a.backend.clone()),
        classification: "required".into(),
        outcome: "FAIL".into(),
        result: Some(ObservedResult::DeterminismFailure),
        failure_class: Some(FailureClass::ProductFailure),
        error_kind: None,
        timeout_seconds: 20,
        execution_cpu_timeout_seconds: Some(10),
        execution_wall_timeout_seconds: Some(20),
        duration_ms: Some(19_000),
        cpu_usage_usec: Some(1_000),
        runtime: None,
        log_level: Some("info".into()),
        effective_args: vec!["run".into()],
        argv: vec!["hermit".into(), "run".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
        relaxations: Vec::new(),
        execution_path: None,
        diversity: None,
        attempts: vec![fixture_attempt("FAIL", 1)],
        reason: None,
        artifact_dir: sample_artifact_dir.to_string_lossy().into_owned(),
    };
    if !result_row_matches_cell(
        &result_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("matching retained result-row identity was refused".into());
    }
    let current_sandbox_runner_results = scratch.join("current-sandbox-runner");
    let current_sandbox_output_dir =
        current_sandbox_runner_results.join(RUNNER_STEP_OUTPUT_DIR);
    fs::create_dir_all(&current_sandbox_output_dir)
        .map_err(|e| format!("cannot create current sandbox runner fixture: {e}"))?;
    let current_sandbox_tag = format!("cell.{sample_slug}");
    let current_sandbox_output_log = PathBuf::from(RUNNER_STEP_OUTPUT_DIR)
        .join(format!("{}.log", sanitize_step_tag(&current_sandbox_tag)));
    fs::write(
        current_sandbox_runner_results.join(&current_sandbox_output_log),
        include_str!("testdata/bpfjailer-pytest-denial.log"),
    )
    .map_err(|e| format!("cannot write current sandbox runner output: {e}"))?;
    fs::write(
        current_sandbox_runner_results.join("runner-outcomes.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&json!({
                "schema": 3,
                "scheduler_passes": 1,
                "outcomes": [{
                    "tag": current_sandbox_tag,
                    "ok": false,
                    "duration_s": 1.0,
                    "returncode": 1,
                    "reason": "failed",
                    "aborted": false,
                    "oomed": false, "oom_kills": 0, "timed_out": false, "cpu_timed_out": false,
                    "output_log": current_sandbox_output_log,
                }],
            }))
            .map_err(|e| format!("cannot encode current sandbox runner fixture: {e}"))?
        ),
    )
    .map_err(|e| format!("cannot write current sandbox runner fixture: {e}"))?;
    let current_sandbox_runner = load_retained_runner_evidence(&current_sandbox_runner_results)?
        .and_then(|evidence| evidence.get(&current_sandbox_tag).copied())
        .ok_or("current sandbox runner fixture was not retained")?;
    let current_sandbox_results = scratch.join("current-sandbox-results.jsonl");
    let mut current_sandbox_row = result_row.clone();
    current_sandbox_row.outcome = "ERROR".into();
    current_sandbox_row.result = Some(ObservedResult::SandboxDenied);
    current_sandbox_row.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
    current_sandbox_row.error_kind = Some("incomplete-verification-evidence".into());
    current_sandbox_row.attempts[0].outcome = "ERROR".into();
    current_sandbox_row.attempts[0].status = Some(1);
    current_sandbox_row.attempts[0].stderr =
        include_str!("testdata/bpfjailer-pytest-denial.log").into();
    fs::write(
        &current_sandbox_results,
        format!(
            "{}\n",
            serde_json::to_string(&current_sandbox_row)
                .map_err(|e| format!("cannot encode current sandbox result fixture: {e}"))?
        ),
    )
    .map_err(|e| format!("cannot write current sandbox result fixture: {e}"))?;
    let current_sandbox_rows = read_result_rows(&current_sandbox_results)?;
    let current_sandbox_row = current_sandbox_rows
        .first()
        .ok_or("current sandbox result fixture was not retained")?;
    if !result_row_matches_cell(
        current_sandbox_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(1),
    ) {
        return Err(
            "current-schema sandbox result was not bound to the exact run SHA and cell identity"
                .into(),
        );
    }
    let retained_bpf_result = classify_result(
        current_sandbox_runner,
        Some(1),
        &current_sandbox_row.outcome,
        true,
        current_sandbox_row.reason.as_deref(),
        &current_sandbox_row.mode,
        Some("no_result"),
        false,
        false,
    );
    if retained_bpf_result != "sandbox-denied"
        || reconcile_recorded_result(
            current_sandbox_row.result,
            current_sandbox_row.failure_class,
            retained_bpf_result,
        )? != "sandbox-denied"
    {
        return Err(
            "current-schema producer result plus retained BPF output did not require sandbox-denied"
                .into(),
        );
    }
    let mut wrongly_typed_product = current_sandbox_row.clone();
    wrongly_typed_product.result = Some(ObservedResult::CrashError);
    wrongly_typed_product.failure_class = Some(FailureClass::ProductFailure);
    let disagreement = reconcile_recorded_result(
        wrongly_typed_product.result,
        wrongly_typed_product.failure_class,
        retained_bpf_result,
    )
    .expect_err("a product result that disagrees with retained BPF evidence was accepted");
    if !disagreement.contains("crash-error") || !disagreement.contains("sandbox-denied") {
        return Err(format!(
            "current-schema sandbox disagreement did not fail by name: {disagreement}"
        ));
    }
    let appended_results = scratch.join("appended-results.jsonl");
    let mut first_row = result_row.clone();
    first_row.first_divergent_record = Some(93);
    first_row.first_divergent_syscall = Some(37);
    first_row.first_divergent_scheduler_turn = Some(68);
    first_row.first_divergent_virtual_nanoseconds = Some(7);
    first_row.first_divergent_left_message = Some("INFO detcore: left event".into());
    first_row.first_divergent_right_message = Some("INFO detcore: right event".into());
    let mut second_row = result_row.clone();
    second_row.attempt = 2;
    second_row.outcome = "PASS".into();
    second_row.result = Some(ObservedResult::Pass);
    second_row.failure_class = None;
    second_row.duration_ms = Some(19_500);
    second_row.timeout_seconds = 20;
    second_row.artifact_dir = format!("{}-attempt-2", first_row.artifact_dir);
    second_row.attempts[0].outcome = "PASS".into();
    second_row.attempts[0].status = Some(0);
    fs::write(
        &appended_results,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&first_row)
                .map_err(|e| format!("cannot encode first appended-row fixture: {e}"))?,
            serde_json::to_string(&second_row)
                .map_err(|e| format!("cannot encode retry appended-row fixture: {e}"))?,
        ),
    )
    .map_err(|e| format!("cannot write appended-row fixture: {e}"))?;
    let appended = read_result_rows(&appended_results)?;
    if appended.len() != 2
        || appended[0].attempt != 1
        || appended[0].result != Some(ObservedResult::DeterminismFailure)
        || appended[0].failure_class != Some(FailureClass::ProductFailure)
        || appended[0].duration_ms != Some(19_000)
        || appended[0].timeout_seconds != 20
        || appended[0].first_divergent_syscall != Some(37)
        || appended[0].first_divergent_left_message.as_deref()
            != Some("INFO detcore: left event")
        || appended[0].first_divergent_right_message.as_deref()
            != Some("INFO detcore: right event")
        || appended[1].attempt != 2
        || appended[1].result != Some(ObservedResult::Pass)
        || appended[1].failure_class.is_some()
        || appended[1].duration_ms != Some(19_500)
        || appended[1].timeout_seconds != 20
        || appended[1].first_divergent_left_message.is_some()
        || appended[1].first_divergent_right_message.is_some()
        || !appended.iter().all(|row| {
            result_row_identity_and_invocation_match(
                row,
                &sample_slug,
                &sample_metadata,
                &sample_a,
                true,
            )
        })
        || result_row_matches_cell(
            &appended[0],
            &sample_slug,
            &sample_metadata,
            &sample_a,
            true,
            Some(0),
        )
        || !result_row_matches_cell(
            &appended[1],
            &sample_slug,
            &sample_metadata,
            &sample_a,
            true,
            Some(0),
        )
        || result_artifact_dir(&scratch, &appended[1])?.as_path()
            != Path::new(&second_row.artifact_dir)
    {
        return Err(format!(
            "two appended result observations were not retained independently: {appended:?}"
        ));
    }
    if read_current_result_rows(&appended_results)?.len() != 2 {
        return Err("the existing current retry history lost strict admission".into());
    }
    let historical_results = scratch.join("historical-timeout-results.jsonl");
    let mut historical_rows = appended.clone();
    for row in &mut historical_rows {
        row.execution_cpu_timeout_seconds = None;
        row.execution_wall_timeout_seconds = None;
    }
    let historical_text = historical_rows.iter().map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?.join("\n") + "\n";
    fs::write(&historical_results, &historical_text).map_err(|error| error.to_string())?;
    if read_result_rows(&historical_results)?.len() != 2 {
        return Err("historical rows without additive timeout fields became unreadable".into());
    }
    let missing_timeout_error = read_current_result_rows(&historical_results)
        .expect_err("historical rows must not satisfy fresh timeout admission");
    if !missing_timeout_error.contains("omitted explicit execution timeout bounds") {
        return Err(format!("historical current-policy refusal had the wrong cause: {missing_timeout_error}"));
    }
    let current_results = scratch.join("current-timeout-results.jsonl");
    let mut current_row = second_row.clone();
    current_row.attempt = 1;
    current_row.timeout_seconds = 57;
    current_row.execution_cpu_timeout_seconds = Some(22);
    current_row.execution_wall_timeout_seconds = Some(57);
    let write_current = |row: &CellResult| -> Result<(), String> {
        fs::write(&current_results, format!("{}\n", serde_json::to_string(row).map_err(|error| error.to_string())?))
            .map_err(|error| error.to_string())
    };
    write_current(&current_row)?;
    if read_current_result_rows(&current_results)?.len() != 1 {
        return Err("valid current timeout result was not admitted".into());
    }
    for (cpu, wall) in [(None, None), (Some(22), None), (None, Some(57)),
        (Some(0), Some(57)), (Some(57), Some(57)), (Some(22), Some(58))]
    {
        let mut malformed = current_row.clone();
        malformed.execution_cpu_timeout_seconds = cpu;
        malformed.execution_wall_timeout_seconds = wall;
        write_current(&malformed)?;
        if read_current_result_rows(&current_results).is_ok() {
            return Err(format!("fresh result accepted malformed timeout pair {cpu:?}/{wall:?}"));
        }
    }
    write_current(&current_row)?;
    let inconsistent_results = scratch.join("inconsistent-results.jsonl");
    let mut inconsistent = first_row.clone();
    inconsistent.failure_class = Some(FailureClass::NoResult);
    fs::write(
        &inconsistent_results,
        format!(
            "{}\n",
            serde_json::to_string(&inconsistent)
                .map_err(|e| format!("cannot encode inconsistent result fixture: {e}"))?
        ),
    )
    .map_err(|e| format!("cannot write inconsistent result fixture: {e}"))?;
    let error = read_result_rows(&inconsistent_results)
        .expect_err("a product result with a no-result attribution must be refused");
    if !error.contains("determinism-failure")
        || !error.contains("ProductFailure")
        || !error.contains("NoResult")
    {
        return Err(format!(
            "classification disagreement did not fail by name: {error}"
        ));
    }
    for nonproduct in [
        FailureClass::UnderstoodInfrastructureFailure,
        FailureClass::UnderstoodPrerequisiteFailure,
        FailureClass::NoResult,
    ] {
        let mut mixed_retry = first_row.clone();
        mixed_retry.attempt = 2;
        mixed_retry.outcome = "ERROR".into();
        mixed_retry.result = None;
        mixed_retry.failure_class = Some(nonproduct);
        if classify_nonpassing_repetition(
            "determinism-failure",
            &[first_row.clone(), mixed_retry],
            true,
            true,
            true,
            false,
            false,
            false,
        ) != RepetitionClassification::Mixed
        {
            return Err(format!(
                "product failure plus {nonproduct:?} retry was promoted to a terminal product classification"
            ));
        }
    }
    if classify_nonpassing_repetition(
        "infrastructure-error",
        &[],
        false,
        false,
        true,
        false,
        false,
        false,
    ) != RepetitionClassification::Missing
    {
        return Err(
            "a rejected duplicate, gapped, empty, or malformed result history counted as an observed infrastructure failure"
                .into(),
        );
    }
    for (proven_timeout, proven_oom) in [(true, false), (false, true)] {
        if classify_nonpassing_repetition(
            "infrastructure-error",
            &[],
            false,
            false,
            false,
            proven_timeout,
            proven_oom,
            false,
        ) != RepetitionClassification::NoResult
        {
            return Err(
                "a proven timeout or OOM without a result row did not remain no-result".into(),
            );
        }
        if classify_nonpassing_repetition(
            "infrastructure-error",
            &[],
            false,
            false,
            true,
            proven_timeout,
            proven_oom,
            false,
        ) != RepetitionClassification::Missing
        {
            return Err(
                "a malformed present result history was hidden by a proven timeout or OOM"
                    .into(),
            );
        }
        if classify_nonpassing_repetition(
            "infrastructure-error",
            &[first_row.clone()],
            false,
            false,
            true,
            proven_timeout,
            proven_oom,
            false,
        ) != RepetitionClassification::Mixed
        {
            return Err(
                "a retained product failure followed by a proven timeout or OOM was not kept mixed"
                    .into(),
            );
        }
    }
    let host_inapplicable_dir = scratch.join("host-inapplicable-summary");
    fs::create_dir_all(&host_inapplicable_dir)
        .map_err(|e| format!("cannot create host-inapplicable summary fixture: {e}"))?;
    fs::write(
        host_inapplicable_dir.join("summary.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": 1,
            "cells": 1,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "host_inapplicable": 1,
            "cell_cpu_usage_usec": null,
            "host_inapplicable_cells": [{
                "test": sample_a.test,
                "mode": sample_a.mode,
                "backend": sample_a.backend,
                "reason": "required host capability is unavailable",
            }],
        }))
        .map_err(|e| format!("cannot encode host-inapplicable summary fixture: {e}"))?,
    )
    .map_err(|e| format!("cannot write host-inapplicable summary fixture: {e}"))?;
    if !retained_host_inapplicable(&host_inapplicable_dir, &sample_a)?
        || classify_nonpassing_repetition(
            "prerequisite-failure",
            &[],
            false,
            true,
            false,
            false,
            false,
            true,
        ) != RepetitionClassification::PrerequisiteFailure
    {
        return Err(
            "canonical host-inapplicable summary was not retained as a prerequisite failure"
                .into(),
        );
    }
    let mismatched_host_inapplicable_dir = scratch.join("mismatched-host-inapplicable-summary");
    fs::create_dir_all(&mismatched_host_inapplicable_dir)
        .map_err(|e| format!("cannot create mismatched host-inapplicable fixture: {e}"))?;
    fs::write(
        mismatched_host_inapplicable_dir.join("summary.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": 1,
            "cells": 1,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "host_inapplicable": 1,
            "host_inapplicable_cells": [{
                "test": "foreign-cell",
                "mode": sample_a.mode,
                "backend": sample_a.backend,
                "reason": "required host capability is unavailable",
            }],
        }))
        .map_err(|e| format!("cannot encode mismatched host-inapplicable fixture: {e}"))?,
    )
    .map_err(|e| format!("cannot write mismatched host-inapplicable fixture: {e}"))?;
    if retained_host_inapplicable(&mismatched_host_inapplicable_dir, &sample_a).is_ok() {
        return Err("a host-inapplicable summary for a foreign cell was accepted".into());
    }
    if retained_attempt_count(
        &appended,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        runner_ok,
        Some(0),
    )? != 2
    {
        return Err("the terminal attempt ordinal did not count both executions".into());
    }
    let unlocated_retry = [result_row.clone(), second_row.clone()];
    if !earlier_attempts_that_located(&unlocated_retry, 2).is_empty()
        || retained_attempt_count(
            &unlocated_retry,
            &sample_slug,
            &sample_metadata,
            &sample_a,
            true,
            runner_ok,
            Some(0),
        )? != 2
    {
        return Err(
            "a retry without divergence coordinates was mistaken for one execution".into(),
        );
    }
    if repetition_passed_cleanly("pass", &appended) {
        return Err(
            "a repetition that failed before its selected pass was counted as cleanly passed"
                .into(),
        );
    }

    // Drive one fail-then-pass retained history through production summarize,
    // not only through its helper functions. This is the load-bearing bracket
    // for both attempt accumulation and the per-repetition retry counter.
    let summarize_retry = unfiltered
        .selected
        .iter()
        .find(|tracked| tracked.id.mode == "naked" && tracked.id.backend == "native")
        .ok_or("self-test needs one selected naked/native red cell")?;
    let summarize_retry_selection = CellSelection {
        test: Some(summarize_retry.id.test.clone()),
        mode: Some(summarize_retry.id.mode.clone()),
        backend: Some(summarize_retry.id.backend.clone()),
        repetitions: Some(1),
        run_id_prefix: Some("summarize-retry".into()),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let summarize_retry_results = scratch.join("summarize-retry");
    let (mut summarize_retry_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &summarize_retry_results,
        &summarize_retry_results.join("dag.json"),
        &summarize_retry_selection,
    )?;
    summarize_retry_metadata.source_tree_dirty = false;
    fs::write(
        summarize_retry_results.join("run.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&summarize_retry_metadata)
                .map_err(|e| format!("cannot encode summarize retry metadata: {e}"))?
        ),
    )
    .map_err(|e| format!("cannot write summarize retry metadata: {e}"))?;
    let summarize_retry_slug = cell_run_slug(&summarize_retry.id, Some(1));
    let summarize_retry_run_id = cell_evidence_run_id(
        &summarize_retry.id,
        Some(1),
        summarize_retry_metadata.run_id_prefix.as_deref(),
    );
    let summarize_retry_cell_dir = summarize_retry_results
        .join("cells")
        .join(&summarize_retry_slug);
    fs::create_dir_all(&summarize_retry_cell_dir)
        .map_err(|e| format!("cannot create summarize retry fixture: {e}"))?;
    fs::write(summarize_retry_cell_dir.join("harness-status"), "0\n")
        .map_err(|e| format!("cannot write summarize retry harness status: {e}"))?;

    let mut summarize_first = result_row.clone();
    summarize_first.run_id = summarize_retry_run_id.clone();
    summarize_first.run_index = Some(1);
    summarize_first.hermit_sha = summarize_retry_metadata.hermit_sha.clone();
    summarize_first.source_tree_dirty = summarize_retry_metadata.source_tree_dirty;
    summarize_first.test = summarize_retry.id.test.clone();
    summarize_first.category = summarize_retry.id.category.clone();
    summarize_first.lane = summarize_retry.id.lane.clone();
    summarize_first.mode = summarize_retry.id.mode.clone();
    summarize_first.backend = Some(summarize_retry.id.backend.clone());
    summarize_first.classification = if summarize_retry.enabled {
        "required".into()
    } else {
        "disabled".into()
    };
    summarize_first.outcome = "FAIL".into();
    summarize_first.result = Some(ObservedResult::CrashError);
    summarize_first.failure_class = Some(FailureClass::ProductFailure);
    summarize_first.reason = Some("planted first-attempt failure".into());
    summarize_first.attempt = 1;
    summarize_first.attempts = vec![fixture_attempt("FAIL", 1)];
    summarize_first.artifact_dir = summarize_retry_results
        .join("runs")
        .join(&summarize_retry_run_id)
        .join("attempt-1")
        .to_string_lossy()
        .into_owned();
    let mut summarize_second = summarize_first.clone();
    summarize_second.outcome = "PASS".into();
    summarize_second.result = Some(ObservedResult::Pass);
    summarize_second.failure_class = None;
    summarize_second.reason = None;
    summarize_second.attempt = 2;
    summarize_second.attempts = vec![fixture_attempt("PASS", 0)];
    summarize_second.artifact_dir = summarize_retry_results
        .join("runs")
        .join(&summarize_retry_run_id)
        .join("attempt-2")
        .to_string_lossy()
        .into_owned();
    fs::write(
        summarize_retry_cell_dir.join("results.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&summarize_first)
                .map_err(|e| format!("cannot encode summarize first attempt: {e}"))?,
            serde_json::to_string(&summarize_second)
                .map_err(|e| format!("cannot encode summarize retry attempt: {e}"))?,
        ),
    )
    .map_err(|e| format!("cannot write summarize retry results: {e}"))?;
    let summarize_runner = BTreeMap::from([(
        format!("cell.{summarize_retry_slug}"),
        runner_ok,
    )]);
    summarize(
        root,
        &summarize_retry_results,
        false,
        Some(&summarize_runner),
        false,
    )?;
    let summarize_json: JsonValue = serde_json::from_str(
        &fs::read_to_string(summarize_retry_results.join("summary.json"))
            .map_err(|e| format!("cannot read production retry summary: {e}"))?,
    )
    .map_err(|e| format!("cannot parse production retry summary: {e}"))?;
    let summarized_cell = summarize_json
        .get("repeated_cells")
        .and_then(JsonValue::as_array)
        .and_then(|cells| cells.first())
        .ok_or("production retry summary lost its repeated cell")?;
    if summarize_json["probe_disabled"] != false
        || summarize_json["attempted"] != 2
        || summarize_json["retried_repetitions"] != 1
        || summarized_cell["passes"] != 1
        || summarized_cell["clean_passes"] != 0
        || summarized_cell["retried_repetitions"] != 1
        || summarized_cell["total"] != 1
        || summarized_cell["result"] != "flaky"
    {
        return Err(format!(
            "production summarize lost fail-then-pass retry accounting: {summarize_json}"
        ));
    }
    // Exercise retained and fresh admission through the real summary boundary.
    // Original current fixture bytes and every outcome assertion above remain
    // intact; a separate historical variant deliberately omits additive fields.
    let summary_metadata_path = summarize_retry_results.join("run.json");
    let summary_dag_path = summarize_retry_results.join("dag.json");
    let summary_result_path = summarize_retry_cell_dir.join("results.jsonl");
    let saved_metadata = fs::read(&summary_metadata_path).map_err(|error| error.to_string())?;
    let saved_dag = fs::read(&summary_dag_path).map_err(|error| error.to_string())?;
    let saved_results = fs::read(&summary_result_path).map_err(|error| error.to_string())?;
    let mut historical_metadata = summarize_retry_metadata.clone();
    historical_metadata.timeout_policy = None;
    let old_budget = manifest_budgets.get(&(
        summarize_retry.id.test.clone(), summarize_retry.id.mode.clone(), summarize_retry.id.backend.clone()
    )).ok_or("historical summary fixture lost its manifest budget")?;
    let mut historical_dag = dag_from_json(std::str::from_utf8(&saved_dag)
        .map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    for step in historical_dag.steps.iter_mut().filter(|step| step.group == "cell") {
        step.timeout = legacy_pressure_timeout(old_budget, historical_metadata.cell_timeout_seconds)?;
        step.cpu_timeout = step.timeout * 2;
    }
    let historical_result_text = [summarize_first.clone(), summarize_second.clone()].into_iter()
        .map(|mut row| {
            row.execution_cpu_timeout_seconds = None;
            row.execution_wall_timeout_seconds = None;
            serde_json::to_string(&row)
        }).collect::<Result<Vec<_>, _>>().map_err(|error| error.to_string())?.join("\n") + "\n";
    fs::write(&summary_metadata_path, serde_json::to_vec(&historical_metadata)
        .map_err(|error| error.to_string())?).map_err(|error| error.to_string())?;
    fs::write(&summary_dag_path, dag_to_json(&historical_dag)).map_err(|error| error.to_string())?;
    fs::write(&summary_result_path, &historical_result_text).map_err(|error| error.to_string())?;
    summarize(root, &summarize_retry_results, false, Some(&summarize_runner), false)?;
    let read_summary = || -> Result<JsonValue, String> {
        serde_json::from_slice(&fs::read(summarize_retry_results.join("summary.json"))
            .map_err(|error| error.to_string())?).map_err(|error| error.to_string())
    };
    if read_summary()? != summarize_json {
        return Err("historical summary changed retained retry outcomes or observations".into());
    }
    let fresh_error = summarize(root, &summarize_retry_results, false, Some(&summarize_runner), true)
        .expect_err("a fresh run cannot omit its policy");
    if !fresh_error.contains("fresh pressure run omitted its timeout policy") {
        return Err(format!("fresh metadata refusal had the wrong cause: {fresh_error}"));
    }
    fs::write(&summary_metadata_path, &saved_metadata).map_err(|error| error.to_string())?;
    fs::write(&summary_dag_path, &saved_dag).map_err(|error| error.to_string())?;
    let missing_error = summarize(root, &summarize_retry_results, false, Some(&summarize_runner), false)
        .expect_err("current retained policy must reject historical timeout omissions");
    let missing_summary = read_summary()?;
    if !missing_error.contains("no trustworthy result")
        || missing_summary["pass_candidates"].as_array().is_none_or(|rows| !rows.is_empty())
        || missing_summary["rows"][0]["result_row_valid"] != false
        || missing_summary["repeated_cells"][0]["passes"] != 0
    {
        return Err(format!("missing current timeout evidence was promoted: {missing_summary}"));
    }
    fs::write(&summary_result_path, &saved_results).map_err(|error| error.to_string())?;
    summarize(root, &summarize_retry_results, false, Some(&summarize_runner), true)?;
    if read_summary()? != summarize_json {
        return Err("restoring exact current evidence changed its fresh summary".into());
    }

    let mut first_pass = summarize_second.clone();
    first_pass.attempt = 1;
    first_pass.attempts = vec![fixture_attempt("PASS", 0)];
    fs::write(&summary_result_path, format!("{}\n", serde_json::to_string(&first_pass)
        .map_err(|error| error.to_string())?)).map_err(|error| error.to_string())?;
    summarize(root, &summarize_retry_results, false, Some(&summarize_runner), true)?;
    let clean_summary = read_summary()?;
    if clean_summary["repeated_cells"][0]["passes"] != 1
        || clean_summary["repeated_cells"][0]["clean_passes"] != 1
        || clean_summary["repeated_cells"][0]["qualifying_passes"] != 1
    {
        return Err(format!("clean first-attempt summary lost qualification: {clean_summary}"));
    }
    let mut adverse_inner = first_pass.clone();
    let first_inner = fixture_attempt("FAIL", 1);
    let mut second_inner = fixture_attempt("PASS", 0); second_inner.index = "2".into();
    adverse_inner.attempts = vec![first_inner, second_inner];
    fs::write(&summary_result_path, format!("{}\n", serde_json::to_string(&adverse_inner)
        .map_err(|error| error.to_string())?)).map_err(|error| error.to_string())?;
    summarize(root, &summarize_retry_results, false, Some(&summarize_runner), true)?;
    let adverse_summary = read_summary()?;
    if adverse_summary["repeated_cells"][0]["passes"] != 1
        || adverse_summary["repeated_cells"][0]["clean_passes"] != 1
        || adverse_summary["repeated_cells"][0]["qualifying_passes"] != 0
        || adverse_summary["repeated_cells"][0]["promotion_candidate"] != false
        || adverse_summary["rows"][0]["result"] != "pass"
        || adverse_summary["rows"][0]["invocation"]["attempts"].as_array().map(Vec::len) != Some(2)
    {
        return Err(format!("outer PASS hid an adverse inner invocation: {adverse_summary}"));
    }
    let mut unknown_inner = first_pass.clone();
    unknown_inner.attempts[0].status = None;
    fs::write(&summary_result_path, format!("{}\n", serde_json::to_string(&unknown_inner)
        .map_err(|error| error.to_string())?)).map_err(|error| error.to_string())?;
    summarize(root, &summarize_retry_results, false, Some(&summarize_runner), true)?;
    let unknown_summary = read_summary()?;
    if unknown_summary["repeated_cells"][0]["passes"] != 1
        || unknown_summary["repeated_cells"][0]["clean_passes"] != 1
        || unknown_summary["repeated_cells"][0]["qualifying_passes"] != 0
        || unknown_summary["repeated_cells"][0]["unknown_history_repetitions"] != 1
        || unknown_summary["repeated_cells"][0]["classification"] != "incomplete"
        || unknown_summary["rows"][0]["result"] != "pass"
    {
        return Err(format!("unknown inner history changed PASS or claimed complete evidence: {unknown_summary}"));
    }
    fs::write(&summary_result_path, &saved_results).map_err(|error| error.to_string())?;

    // HOST-INAPPLICABLE is deliberately withheld from results.jsonl by the
    // manifest runner because no product attempt ran. Its one-cell harness
    // summary is therefore the canonical retained prerequisite evidence.
    let mut prerequisite_selection = summarize_retry_selection.clone();
    prerequisite_selection.run_id_prefix = Some("summarize-prerequisite".into());
    let prerequisite_results = scratch.join("summarize-prerequisite");
    let (mut prerequisite_metadata, _) = write_plan_after_scorecard_check(
        &checked_scorecard,
        &prerequisite_results,
        &prerequisite_results.join("dag.json"),
        &prerequisite_selection,
    )?;
    prerequisite_metadata.source_tree_dirty = false;
    fs::write(
        prerequisite_results.join("run.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&prerequisite_metadata)
                .map_err(|e| format!("cannot encode prerequisite metadata: {e}"))?
        ),
    )
    .map_err(|e| format!("cannot write prerequisite metadata: {e}"))?;
    let prerequisite_slug = cell_run_slug(&summarize_retry.id, Some(1));
    let prerequisite_cell_dir = prerequisite_results
        .join("cells")
        .join(&prerequisite_slug);
    fs::create_dir_all(&prerequisite_cell_dir)
        .map_err(|e| format!("cannot create prerequisite fixture: {e}"))?;
    fs::write(prerequisite_cell_dir.join("harness-status"), "1\n")
        .map_err(|e| format!("cannot write prerequisite harness status: {e}"))?;
    fs::write(prerequisite_cell_dir.join("results.jsonl"), "")
        .map_err(|e| format!("cannot prepare empty prerequisite result file: {e}"))?;
    fs::write(
        prerequisite_cell_dir.join("summary.json"),
        serde_json::to_vec_pretty(&json!({
            "schema": 1,
            "cells": 1,
            "passed": 0,
            "failed": 0,
            "errors": 0,
            "host_inapplicable": 1,
            "cell_cpu_usage_usec": null,
            "host_inapplicable_cells": [{
                "test": summarize_retry.id.test,
                "mode": summarize_retry.id.mode,
                "backend": summarize_retry.id.backend,
                "reason": "required host capability is unavailable",
            }],
        }))
        .map_err(|e| format!("cannot encode prerequisite harness summary: {e}"))?,
    )
    .map_err(|e| format!("cannot write prerequisite harness summary: {e}"))?;
    let prerequisite_runner = BTreeMap::from([(
        format!("cell.{prerequisite_slug}"),
        runner_failed,
    )]);
    let prerequisite_error = summarize(
        root,
        &prerequisite_results,
        false,
        Some(&prerequisite_runner),
        false,
    ).expect_err("a prerequisite sample must preserve the existing no-product-result refusal");
    if !prerequisite_error.contains("no trustworthy result") {
        return Err(format!("unexpected prerequisite refusal: {prerequisite_error}"));
    }
    let prerequisite_json: JsonValue = serde_json::from_str(
        &fs::read_to_string(prerequisite_results.join("summary.json"))
            .map_err(|e| format!("cannot read production prerequisite summary: {e}"))?,
    )
    .map_err(|e| format!("cannot parse production prerequisite summary: {e}"))?;
    let prerequisite_cell = prerequisite_json
        .get("repeated_cells")
        .and_then(JsonValue::as_array)
        .and_then(|cells| cells.first())
        .ok_or("production prerequisite summary lost its repeated cell")?;
    if prerequisite_json["attempted"] != 1
        || prerequisite_cell["observed_repetitions"] != 1
        || prerequisite_cell["prerequisite_failures"] != 1
        || prerequisite_cell["missing_repetitions"] != 0
        || prerequisite_cell["classification"] != "incomplete"
        || prerequisite_cell["promotion_candidate"] != false
    {
        return Err(format!(
            "production summarize did not preserve host-inapplicable prerequisite evidence: {prerequisite_json}"
        ));
    }

    let summarize_empty_terminal =
        |name: &str, runner: RunnerEvidence, status: i32| -> Result<JsonValue, String> {
            let mut selection = summarize_retry_selection.clone();
            selection.run_id_prefix = Some(name.into());
            let result_dir = scratch.join(name);
            let (mut metadata, _) = write_plan_after_scorecard_check(
                &checked_scorecard,
                &result_dir,
                &result_dir.join("dag.json"),
                &selection,
            )?;
            metadata.source_tree_dirty = false;
            fs::write(
                result_dir.join("run.json"),
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&metadata)
                        .map_err(|e| format!("cannot encode {name} metadata: {e}"))?
                ),
            )
            .map_err(|e| format!("cannot write {name} metadata: {e}"))?;
            let slug = cell_run_slug(&summarize_retry.id, Some(1));
            let cell_dir = result_dir.join("cells").join(&slug);
            fs::create_dir_all(&cell_dir)
                .map_err(|e| format!("cannot create {name} fixture: {e}"))?;
            fs::write(cell_dir.join("harness-status"), format!("{status}\n"))
                .map_err(|e| format!("cannot write {name} harness status: {e}"))?;
            fs::write(cell_dir.join("results.jsonl"), "")
                .map_err(|e| format!("cannot prepare empty {name} result file: {e}"))?;
            let runner_evidence = BTreeMap::from([(format!("cell.{slug}"), runner)]);
            let refusal = summarize(root, &result_dir, false, Some(&runner_evidence), false)
                .expect_err("an empty timed-out sample must preserve the existing result refusal");
            if !refusal.contains("no trustworthy result") {
                return Err(format!("unexpected empty terminal refusal: {refusal}"));
            }
            serde_json::from_str(
                &fs::read_to_string(result_dir.join("summary.json"))
                    .map_err(|e| format!("cannot read production {name} summary: {e}"))?,
            )
            .map_err(|e| format!("cannot parse production {name} summary: {e}"))
        };
    for (name, runner, status) in [
        (
            "summarize-empty-timeout",
            runner_timeout,
            INCOMPLETE_ATTEMPT_STATUS,
        ),
        ("summarize-empty-oom", runner_oom, 137),
    ] {
        let terminal_json = summarize_empty_terminal(name, runner, status)?;
        let terminal_cell = terminal_json
            .get("repeated_cells")
            .and_then(JsonValue::as_array)
            .and_then(|cells| cells.first())
            .ok_or_else(|| format!("production {name} summary lost its repeated cell"))?;
        if terminal_json["attempted"] != 1
            || terminal_cell["observed_repetitions"] != 1
            || terminal_cell["no_results"] != 1
            || terminal_cell["missing_repetitions"] != 0
            || terminal_cell["classification"] != "incomplete"
            || terminal_cell["promotion_candidate"] != false
        {
            return Err(format!(
                "production summarize did not preserve empty-file {name} no-result evidence: {terminal_json}"
            ));
        }
    }
    let mut one_pass = second_row.clone();
    one_pass.attempt = 1;
    if !repetition_passed_cleanly("pass", &[one_pass]) {
        return Err("a one-attempt passing repetition was not counted as passed".into());
    }
    let repeated_counts = |expected, terminal, qualifying, product, retried| {
        RepeatedOutcomeCounts {
            expected_repetitions: expected,
            observed_repetitions: expected,
            qualifying_passes: qualifying,
            clean_passes: qualifying,
            terminal_passes: terminal,
            product_failures: product,
            retried_repetitions: retried,
            ..RepeatedOutcomeCounts::default()
        }
    };
    let retry_summary = repeated_cell_summary(
        &sample_a,
        repeated_counts(2, 2, 1, 0, 1),
        "flaky",
    );
    if retry_summary["passes"] != 2
        || retry_summary["clean_passes"] != 1
        || retry_summary["retried_repetitions"] != 1
        || retry_summary["total"] != 2
        || retry_summary["result"] != "flaky"
    {
        return Err("repeated-cell JSON lost pass, retry, total, or result accounting".into());
    }
    let one_recovered = repeated_cell_summary(
        &sample_a,
        repeated_counts(1, 1, 0, 0, 1),
        "flaky",
    );
    let all_recovered = repeated_cell_summary(
        &sample_a,
        repeated_counts(2, 2, 0, 0, 2),
        "flaky",
    );
    let all_terminal_failures = repeated_cell_summary(
        &sample_a,
        repeated_counts(2, 0, 0, 2, 2),
        "failed every repetition",
    );
    let exact_recovered_json = json!({
        "probe_disabled": false,
        "attempted": 2,
        "retried_repetitions": 1,
        "repeated_result": top_level_repeated_result_description(
            &repeated_metadata,
            1,
            0,
            0,
            1,
            1,
        ),
        "repeated_cells": [one_recovered],
    });
    let exact_all_recovered_json = json!({
        "probe_disabled": false,
        "attempted": 4,
        "retried_repetitions": 2,
        "repeated_result": top_level_repeated_result_description(
            &repeated_metadata,
            2,
            0,
            0,
            2,
            2,
        ),
        "repeated_cells": [all_recovered.clone()],
    });
    let batch_one_recovered_json = json!({
        "probe_disabled": false,
        "attempted": 3,
        "retried_repetitions": 1,
        "repeated_result": top_level_repeated_result_description(
            &red_batch_result_metadata,
            2,
            1,
            0,
            1,
            2,
        ),
        "repeated_cells": [retry_summary.clone()],
    });
    let batch_recovered_json = json!({
        "probe_disabled": false,
        "attempted": 4,
        "retried_repetitions": 2,
        "repeated_result": top_level_repeated_result_description(
            &red_batch_result_metadata,
            2,
            0,
            0,
            2,
            2,
        ),
        "repeated_cells": [all_recovered],
    });
    let exact_failed_json = json!({
        "probe_disabled": false,
        "attempted": 4,
        "retried_repetitions": 2,
        "repeated_result": top_level_repeated_result_description(
            &repeated_metadata,
            0,
            0,
            0,
            2,
            2,
        ),
        "repeated_cells": [all_terminal_failures.clone()],
    });
    let batch_failed_json = json!({
        "probe_disabled": false,
        "attempted": 4,
        "retried_repetitions": 2,
        "repeated_result": top_level_repeated_result_description(
            &red_batch_result_metadata,
            0,
            0,
            0,
            2,
            2,
        ),
        "repeated_cells": [all_terminal_failures],
    });
    if exact_recovered_json["repeated_result"] != "flaky"
        || exact_recovered_json["repeated_cells"][0]["passes"] != 1
        || exact_recovered_json["repeated_cells"][0]["clean_passes"] != 0
        || exact_recovered_json["repeated_cells"][0]["result"] != "flaky"
        || exact_all_recovered_json["repeated_result"] != "flaky"
        || exact_all_recovered_json["repeated_cells"][0]["passes"] != 2
        || exact_all_recovered_json["repeated_cells"][0]["clean_passes"] != 0
        || exact_all_recovered_json["repeated_cells"][0]["result"] != "flaky"
        || batch_one_recovered_json["repeated_result"]
            != "one or more repeated checks failed or required a retry"
        || batch_one_recovered_json["repeated_cells"][0]["passes"] != 2
        || batch_one_recovered_json["repeated_cells"][0]["clean_passes"] != 1
        || batch_one_recovered_json["repeated_cells"][0]["result"] != "flaky"
        || batch_recovered_json["repeated_result"]
            != "one or more repeated checks failed or required a retry"
        || batch_recovered_json["repeated_cells"][0]["passes"] != 2
        || batch_recovered_json["repeated_cells"][0]["clean_passes"] != 0
        || batch_recovered_json["repeated_cells"][0]["result"] != "flaky"
        || exact_failed_json["repeated_result"] != "failed every repetition"
        || exact_failed_json["repeated_cells"][0]["passes"] != 0
        || exact_failed_json["repeated_cells"][0]["clean_passes"] != 0
        || exact_failed_json["repeated_cells"][0]["result"] != "failed every repetition"
        || batch_failed_json["repeated_result"]
            != "one or more repeated checks failed or required a retry"
        || batch_failed_json["repeated_cells"][0]["passes"] != 0
        || batch_failed_json["repeated_cells"][0]["clean_passes"] != 0
        || batch_failed_json["repeated_cells"][0]["result"] != "failed every repetition"
    {
        return Err(
            "exact or batch JSON confused recovered retries with terminal failures".into(),
        );
    }
    verify_repetition_summary_json(&exact_recovered_json, 2, 1)?;
    verify_repetition_summary_json(&exact_all_recovered_json, 4, 2)?;
    verify_repetition_summary_json(&batch_one_recovered_json, 3, 1)?;
    verify_repetition_summary_json(&batch_recovered_json, 4, 2)?;
    verify_repetition_summary_json(&exact_failed_json, 4, 2)?;
    verify_repetition_summary_json(&batch_failed_json, 4, 2)?;
    let summary_accounting = json!({
        "probe_disabled": true,
        "attempted": 3,
        "retried_repetitions": 1,
        "repeated_cells": [retry_summary],
    });
    verify_repetition_summary_json(&summary_accounting, 3, 1)?;
    let mut missing_population_identity = summary_accounting.clone();
    missing_population_identity
        .as_object_mut()
        .expect("summary fixture is an object")
        .remove("probe_disabled");
    let mut missing_retry_count = summary_accounting.clone();
    missing_retry_count
        .as_object_mut()
        .expect("summary fixture is an object")
        .remove("retried_repetitions");
    let mut wrong_attempt_count = summary_accounting.clone();
    wrong_attempt_count["attempted"] = json!(2);
    let mut incomplete_cell = summary_accounting.clone();
    incomplete_cell["repeated_cells"][0]
        .as_object_mut()
        .expect("repeated-cell fixture is an object")
        .remove("retried_repetitions");
    let mut impossible_cell = missing_retry_count.clone();
    impossible_cell["retried_repetitions"] = json!(1);
    impossible_cell["repeated_cells"][0]["retried_repetitions"] = json!(3);
    let mut forged_promotion = summary_accounting.clone();
    forged_promotion["repeated_cells"][0]["classification"] = json!("promotion-candidate");
    forged_promotion["repeated_cells"][0]["promotion_candidate"] = json!(true);
    if verify_repetition_summary_json(&missing_population_identity, 3, 1).is_ok()
        || verify_repetition_summary_json(&missing_retry_count, 3, 1).is_ok()
        || verify_repetition_summary_json(&wrong_attempt_count, 3, 1).is_ok()
        || verify_repetition_summary_json(&incomplete_cell, 3, 1).is_ok()
        || verify_repetition_summary_json(&impossible_cell, 3, 1).is_ok()
        || verify_repetition_summary_json(&forged_promotion, 3, 1).is_ok()
    {
        return Err("mutated repetition-accounting JSON was accepted".into());
    }
    let nested_results = scratch.join("series-layout");
    let nested_cell = nested_results
        .join("cells")
        .join("portable-sample-a-verify-ptrace-repetition-0004");
    fs::create_dir_all(&nested_cell)
        .map_err(|e| format!("cannot create nested series result fixture: {e}"))?;
    let mut nested_first = first_row.clone();
    nested_first.run_index = Some(4);
    let mut nested_second = second_row.clone();
    nested_second.run_index = Some(4);
    let retained_log = scratch.join("retained-verification.log");
    fs::write(
        &retained_log,
        "Internally, the hermit scheduler ran 12 turns, recorded 0 events, replayed 0 events (0 desynced)\nElapsed virtual global (cpu) time: 34ns\nINFO [detcore, dtid 7] finish syscall #5\n",
    )
    .map_err(|e| format!("cannot write retained verification log fixture: {e}"))?;
    nested_first.attempts[0].stderr = format!(
        "::   run 1: {}\n::   run 2: {}",
        retained_log.display(),
        retained_log.display(),
    );
    let typed_runtime = VerificationRuntime {
        run1: Some(RuntimeStats {
            scheduler_turns: 13,
            virtual_nanoseconds: 35,
            syscalls: Some(6),
        }),
        run2: None,
    };
    nested_second.runtime = Some(typed_runtime.clone());
    fs::write(
        nested_cell.join("results.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&nested_first)
                .map_err(|e| format!("cannot encode first nested-row fixture: {e}"))?,
            serde_json::to_string(&nested_second)
                .map_err(|e| format!("cannot encode second nested-row fixture: {e}"))?,
        ),
    )
    .map_err(|e| format!("cannot write nested series result fixture: {e}"))?;
    if collect_series_rows(&nested_results, true)?.len() != 2 {
        return Err("the existing current nested series lost strict admission".into());
    }
    let historical_series = scratch.join("historical-series-layout");
    let historical_cell = historical_series.join("cells").join(&sample_slug);
    fs::create_dir_all(&historical_cell).map_err(|error| error.to_string())?;
    fs::write(historical_cell.join("results.jsonl"), &historical_text).map_err(|error| error.to_string())?;
    if collect_series_rows(&historical_series, false)?.len() != 2 {
        return Err("historical series rows became unreadable".into());
    }
    let strict_series_error = collect_series_rows(&historical_series, true)
        .expect_err("historical series rows must not satisfy fresh admission");
    if !strict_series_error.contains("omitted explicit execution timeout bounds") {
        return Err(format!("strict series admission refused for the wrong cause: {strict_series_error}"));
    }
    let nested_rows = collect_series_rows(&nested_results, false)?;
    if nested_rows.len() != 2
        || nested_rows[0].1.run_index != Some(4)
        || nested_rows[0].1.attempt != 1
        || nested_rows[0].1.first_divergent_left_message.as_deref()
            != Some("INFO detcore: left event")
        || nested_rows[0].1.first_divergent_right_message.as_deref()
            != Some("INFO detcore: right event")
        || nested_rows[0].1.runtime.is_some()
        || nested_rows[1].1.run_index != Some(4)
        || nested_rows[1].1.attempt != 2
        || nested_rows[1].1.runtime.as_ref() != Some(&typed_runtime)
    {
        return Err(format!(
            "pressure series writer did not retain typed runtime and honest absence from the ordinary nested layout: {nested_rows:?}"
        ));
    }
    nested_second.run_index = Some(3);
    fs::write(
        nested_cell.join("results.jsonl"),
        format!(
            "{}\n{}\n",
            serde_json::to_string(&nested_first)
                .map_err(|e| format!("cannot encode matching nested-row fixture: {e}"))?,
            serde_json::to_string(&nested_second)
                .map_err(|e| format!("cannot encode mismatched nested-row fixture: {e}"))?,
        ),
    )
    .map_err(|e| format!("cannot write mismatched nested series fixture: {e}"))?;
    if collect_series_rows(&nested_results, false).is_ok() {
        return Err("a framework result whose run_index disagreed with its pressure directory was accepted".into());
    }
    let mut reused_artifact = second_row.clone();
    reused_artifact.artifact_dir = first_row.artifact_dir.clone();
    fs::write(
        &appended_results,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&first_row)
                .map_err(|e| format!("cannot encode reused-artifact fixture: {e}"))?,
            serde_json::to_string(&reused_artifact)
                .map_err(|e| format!("cannot encode reused-artifact fixture: {e}"))?,
        ),
    )
    .map_err(|e| format!("cannot write reused-artifact fixture: {e}"))?;
    if read_result_rows(&appended_results).is_ok() {
        return Err("two result attempts were allowed to reuse one artifact directory".into());
    }
    fs::write(
        &appended_results,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&first_row)
                .map_err(|e| format!("cannot encode duplicate-attempt fixture: {e}"))?,
            serde_json::to_string(&first_row)
                .map_err(|e| format!("cannot encode duplicate-attempt fixture: {e}"))?,
        ),
    )
    .map_err(|e| format!("cannot write duplicate-attempt fixture: {e}"))?;
    if read_result_rows(&appended_results).is_ok() {
        return Err("duplicate appended result attempts were accepted".into());
    }
    fs::write(&appended_results, "{\n")
        .map_err(|e| format!("cannot write malformed appended-row fixture: {e}"))?;
    if read_result_rows(&appended_results).is_ok() {
        return Err("a malformed appended result row was accepted".into());
    }
    result_row.attempts[0].argv = vec!["hermit".into(), "run".into(), "--hidden-policy".into()];
    if result_row_matches_cell(
        &result_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("a result row whose published argv differs from execution was accepted".into());
    }
    result_row.attempts[0].argv = vec!["hermit".into(), "run".into()];
    result_row.shell_command = "true".into();
    result_row.attempts[0].shell_command = "true".into();
    if result_row_matches_cell(
        &result_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("a result row whose shell command does not encode argv/env was accepted".into());
    }
    result_row.shell_command = "cd /repo && env LC_ALL=C hermit run".into();
    result_row.attempts[0].shell_command = "cd /repo && env LC_ALL=C hermit run".into();
    result_row.hermit_sha = "foreign".into();
    if result_row_matches_cell(
        &result_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("foreign retained result-row identity was accepted".into());
    }
    result_row.attempt = 2;
    let mixed_identity_retry = [first_row.clone(), result_row.clone()];
    if retained_attempt_count(
        &mixed_identity_retry,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        runner_ok,
        Some(0),
    )? != 1
    {
        return Err("a foreign retained retry changed the selected cell's attempt count".into());
    }
    let first_repetition_slug = cell_run_slug(&green_id, Some(1));
    let second_repetition_slug = cell_run_slug(&green_id, Some(2));
    let repeated_result_row = CellResult {
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        attempt: 1,
        schema: CELL_RESULT_SCHEMA,
        run_id: first_repetition_slug.clone(),
        run_index: Some(1),
        machine_shortname: "fixture-host".into(),
        kernel_version: "7.1.3-fixture".into(),
        host_capabilities: fixture_host_capabilities(),
        hermit_sha: repeated_metadata.hermit_sha.clone(),
        source_tree_dirty: false,
        binary_sha256: None,
        binary_build_sha: None,
        test_sha256: "fixture-test-sha256".into(),
        test: green_id.test.clone(),
        category: green_id.category.clone(),
        lane: green_id.lane.clone(),
        mode: green_id.mode.clone(),
        backend: Some(green_id.backend.clone()),
        classification: "required".into(),
        outcome: "PASS".into(),
        result: Some(ObservedResult::Pass),
        failure_class: None,
        error_kind: None,
        timeout_seconds: 20,
        execution_cpu_timeout_seconds: Some(10),
        execution_wall_timeout_seconds: Some(20),
        duration_ms: Some(1_000),
        cpu_usage_usec: Some(1_000),
        runtime: None,
        log_level: Some("info".into()),
        effective_args: vec!["run".into()],
        argv: vec!["hermit".into(), "run".into()],
        guest_argv: vec!["fixture".into()],
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        cwd: "/repo".into(),
        shell_command: "cd /repo && env LC_ALL=C hermit run".into(),
        relaxations: Vec::new(),
        execution_path: None,
        diversity: None,
        attempts: vec![fixture_attempt("PASS", 0)],
        reason: None,
        artifact_dir: scratch
            .join("runs")
            .join(&first_repetition_slug)
            .join("green-a-verify-ptrace")
            .to_string_lossy()
            .into_owned(),
    };
    if !result_row_matches_cell(
        &repeated_result_row,
        &first_repetition_slug,
        &repeated_metadata,
        &green_id,
        true,
        Some(0),
    ) || result_row_matches_cell(
        &repeated_result_row,
        &second_repetition_slug,
        &repeated_metadata,
        &green_id,
        true,
        Some(0),
    ) {
        return Err("retained result-row evidence crossed between repetitions".into());
    }

    if !retained_verification_logs(&sample_a, &sample_artifact_dir)?.is_empty() {
        return Err("missing verify-log directory produced retained logs".into());
    }
    let verification_path = verification_report_path(&sample_artifact_dir);
    let verification_directory = verification_path
        .parent()
        .expect("verification path has parent");
    let verify_log_directory = verification_directory.join("verify-logs").join("verify-1");
    fs::create_dir_all(&verify_log_directory)
        .map_err(|e| format!("cannot create verify-log self-test directory: {e}"))?;
    let run1_log = verify_log_directory.join("run1_log_fixture.log");
    let run2_log = verify_log_directory.join("run2_log_fixture.log");
    fs::write(&run1_log, "run one\n")
        .map_err(|e| format!("cannot write run1 verify-log fixture: {e}"))?;
    if retained_verification_logs(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("retained verify-log evidence accepted a missing run2 capture".into());
    }
    fs::write(&run2_log, "run two\n")
        .map_err(|e| format!("cannot write run2 verify-log fixture: {e}"))?;
    if retained_verification_logs(&sample_a, &sample_artifact_dir)?.len() != 2 {
        return Err("one nonempty run1/run2 verify-log pair was refused".into());
    }
    let duplicate_run1 = verify_log_directory.join("run1_log_duplicate.log");
    fs::write(&duplicate_run1, "duplicate\n")
        .map_err(|e| format!("cannot write duplicate run1 fixture: {e}"))?;
    if retained_verification_logs(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("duplicate retained run1 verify-log capture was accepted".into());
    }
    fs::remove_file(&duplicate_run1)
        .map_err(|e| format!("cannot remove duplicate run1 fixture: {e}"))?;
    fs::write(&run2_log, "").map_err(|e| format!("cannot empty run2 verify-log fixture: {e}"))?;
    if retained_verification_logs(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("empty retained run2 verify-log capture was accepted".into());
    }
    fs::write(&run2_log, "run two\n")
        .map_err(|e| format!("cannot restore run2 verify-log fixture: {e}"))?;

    let golden_status = verify_log_directory.join("normalized-ptrace-golden.status");
    let golden_log = verify_log_directory.join("normalized-ptrace-golden.log");
    if normalized_ptrace_golden(&sample_a, &sample_artifact_dir)?.is_some() {
        return Err("absent normalized ptrace golden produced an artifact".into());
    }
    fs::write(&golden_log, "canonical INFO\n")
        .map_err(|e| format!("cannot write normalized golden fixture: {e}"))?;
    if normalized_ptrace_golden(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("normalized ptrace golden without status was accepted".into());
    }
    fs::remove_file(&golden_log)
        .map_err(|e| format!("cannot remove normalized golden fixture: {e}"))?;
    fs::write(&golden_status, "0\n")
        .map_err(|e| format!("cannot write normalized golden status: {e}"))?;
    if normalized_ptrace_golden(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("normalized ptrace golden status without output was accepted".into());
    }
    fs::write(&golden_log, "canonical INFO\n")
        .map_err(|e| format!("cannot restore normalized golden fixture: {e}"))?;
    if normalized_ptrace_golden(&sample_a, &sample_artifact_dir)?.is_none() {
        return Err("complete normalized ptrace golden output/status pair was refused".into());
    }
    fs::write(&golden_status, "not-a-status\n")
        .map_err(|e| format!("cannot mutate normalized golden status: {e}"))?;
    if normalized_ptrace_golden(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("nonnumeric normalized ptrace golden status was accepted".into());
    }

    fs::write(&verification_path, "{")
        .map_err(|e| format!("cannot write malformed verification fixture: {e}"))?;
    if read_verification_report(&sample_a, &sample_artifact_dir).is_ok() {
        return Err("malformed existing verification report was accepted".into());
    }
    fs::remove_file(&verification_path)
        .map_err(|e| format!("cannot remove malformed verification fixture: {e}"))?;

    let invalid_profile = profile_dir.join("step_profiles_invalid.csv");
    fs::write(
        &invalid_profile,
        "git_sha,step,ok,timed_out,cpu_timed_out,oom_kills\nabc,cell.bad,Maybe,False,False,0\n",
    )
    .map_err(|e| format!("cannot write invalid self-test runner profile: {e}"))?;
    if load_runner_evidence(&scratch, "abc").is_ok() {
        return Err("malformed retained runner evidence was accepted".into());
    }
    clone_source_cleanup.remove()?;
    scratch_cleanup.remove()?;
    println!(
        "compatibility pressure-test self-test: no-hardlinks exact checkout, scorecard/manifest refusal, direct scheduler, multi-failure continuation, red/green and cells-file selection, exact and batch repetitions, retry/attempt/JSON accounting, minimum shared build/preparation, sampling, timeout/OOM classification, generated-DAG mutation, cleanup, retained-runner/result identity, verify-log, and normalized-golden brackets pass"
    );
    Ok(())
}

#[cfg(test)]
mod typed_termination_tests {
    use super::*;

    #[test]
    fn retained_schema_and_typed_facts_refuse_malformed_evidence() {
        let path = env::temp_dir().join(format!(
            "hermit-pressure-self-test-typed-termination-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir(&path).unwrap();
        let guard = SelfTestDirectory::new(path.clone());
        retained_termination_self_test(&path).unwrap();
        guard.remove().unwrap();
    }
}

#[cfg(test)]
mod pressure_sample_tests {
    use super::*;

    #[test]
    fn fixed_ten_sample_categories_preserve_missing_retry_and_mixed_evidence() {
        pressure_sample_classification_self_test().unwrap();
    }

    #[test]
    fn declared_subruns_must_all_pass_before_sample_qualification() {
        let first = fixture_attempt("PASS", 0);
        let mut second = first.clone();
        second.index = "2".into();
        assert!(qualifying_subruns(
            "naked",
            &[first.clone(), second.clone()]
        ));
        assert!(!qualifying_subruns("naked", &[]));
        for outcome in ["FAIL", "ERROR", "HOST-INAPPLICABLE", "UNKNOWN"] {
            let mut failed = first.clone();
            failed.outcome = outcome.into();
            assert!(
                !qualifying_subruns("naked", &[failed, second.clone()]),
                "{outcome}"
            );
        }
        let mut timed_out = first.clone();
        timed_out.timed_out = true;
        assert!(!qualifying_subruns("naked", &[timed_out, second.clone()]));
        let mut error = first.clone();
        error.error_kind = Some("infrastructure".into());
        assert!(!qualifying_subruns("naked", &[error, second]));
        assert!(!qualifying_subruns("naked", &[first.clone(), first]));
        for mode in ["naked", "custom"] {
            let mut expected_nonzero = no_result_attempt("not_run", Some("cpu-timeout"));
            expected_nonzero.outcome = "PASS".into();
            expected_nonzero.status = Some(17);
            expected_nonzero.error_kind = None;
            let before = serde_json::to_value(&expected_nonzero).unwrap();
            assert!(qualifying_subruns(
                mode,
                std::slice::from_ref(&expected_nonzero)
            ));
            assert!(
                retained_pressure_attempt(mode, &expected_nonzero)
                    .unwrap()
                    .comparison
                    .is_none()
            );
            assert_eq!(serde_json::to_value(&expected_nonzero).unwrap(), before);
            expected_nonzero.status = None;
            expected_nonzero.signal = Some(11);
            assert!(qualifying_subruns(mode, &[expected_nonzero]));
            let mut timeout = no_result_attempt("not_run", Some("cpu-timeout"));
            timeout.timed_out = true;
            timeout.status = None;
            assert_eq!(
                inner_pressure_category(&retained_pressure_attempt(mode, &timeout).unwrap()),
                Some(RepetitionClassification::NoResult)
            );
            assert!(!qualifying_subruns(mode, &[timeout]));
        }
    }

    fn comparison_attempt(mode: &str, status: i32) -> AttemptResult {
        let mut attempt = fixture_attempt("PASS", status);
        let mut report = serde_json::to_value(VerificationReport::no_result()).unwrap();
        report["verified"] = json!(true);
        report["bitwise_parity"] = json!(true);
        report["verdict"] = json!("matched");
        report["no_result_reason"] = JsonValue::Null;
        report["guest_exit_code"] = json!(status);
        report["compared_log_messages"] = json!({"left": 17, "right": 17});
        report["comparison"] = json!({
            "strictness":"canonical", "display_name":"BitwiseInfoV1", "compare_logs":true,
            "compare_io_buffers":true, "log_scope":"info", "record_envelope":"all_records_v1",
            "virtualize_time":mode != "replay", "strip_lines":false,
            "canonicalize_addresses":true, "full_trace":true, "exact_remainder":true,
            "stripped_prefixes":["real-wall-clock-prefix/v1"],
            "canonicalizations":["host-address-to-first-appearance-ordinal/v1"],
            "ignore_lines":false, "skip_commit":false, "skip_detlog":false
        });
        replace_report(&mut attempt, report);
        attempt
    }

    fn replace_report(attempt: &mut AttemptResult, report: JsonValue) {
        let raw = serde_json::to_string(&report).unwrap();
        attempt.verification_report_sha256 =
            Some(format!("{:x}", sha2::Sha256::digest(raw.as_bytes())));
        attempt.verification_report = Some(raw);
    }

    #[test]
    fn comparison_subruns_require_every_current_report_hash_and_process() {
        for (mode, status) in [("verify", 0), ("replay", 0), ("chaos", 17)] {
            let first = comparison_attempt(mode, status);
            let mut second = first.clone();
            second.index = "2".into();
            assert!(
                qualifying_subruns(mode, &[first.clone(), second.clone()]),
                "{mode}"
            );
            let mut absent = first.clone();
            absent.verification_report = None;
            assert!(
                !qualifying_subruns(mode, &[absent, second.clone()]),
                "{mode}: missing first report"
            );
            let mut wrong = first.clone();
            wrong.verification_report_sha256 = Some("0".repeat(64));
            assert!(
                !qualifying_subruns(mode, &[wrong, second.clone()]),
                "{mode}: wrong first digest"
            );
            let mut signal = first.clone();
            signal.signal = Some(9);
            assert!(
                !qualifying_subruns(mode, &[signal, second.clone()]),
                "{mode}: contradictory process"
            );
            for (field, value) in [
                ("guest_exit_code", json!(-1)),
                ("guest_signal", json!(0)),
                ("guest_signal", json!(9)),
                ("guest_exit_code", json!("0")),
            ] {
                let mut changed = first.clone();
                let mut report: JsonValue =
                    serde_json::from_str(changed.verification_report.as_ref().unwrap()).unwrap();
                report[field] = value;
                replace_report(&mut changed, report);
                assert!(
                    retained_pressure_attempt(mode, &changed).is_err(),
                    "{mode}: {field}"
                );
                assert!(!qualifying_subruns(mode, &[changed, second.clone()]));
            }
            for (field, value) in [
                ("strictness", json!("stripped")),
                ("compare_io_buffers", json!(false)),
                ("full_trace", json!(false)),
                ("exact_remainder", json!(false)),
                ("skip_commit", json!(true)),
                ("virtualize_time", json!(mode == "replay")),
                ("display_name", json!("other")),
                ("log_scope", json!("detlog")),
                ("ignore_lines", json!(true)),
                ("strip_lines", json!(true)),
            ] {
                let mut changed = first.clone();
                let mut report: JsonValue =
                    serde_json::from_str(changed.verification_report.as_ref().unwrap()).unwrap();
                report["comparison"][field] = value;
                replace_report(&mut changed, report);
                assert!(
                    !qualifying_subruns(mode, &[changed, second.clone()]),
                    "{mode}: {field}"
                );
            }
            for counts in [json!({"left":0,"right":0}), json!({"left":17,"right":18})] {
                let mut changed = first.clone();
                let mut report: JsonValue =
                    serde_json::from_str(changed.verification_report.as_ref().unwrap()).unwrap();
                report["compared_log_messages"] = counts;
                replace_report(&mut changed, report);
                assert!(
                    !qualifying_subruns(mode, &[changed, second.clone()]),
                    "{mode}: invalid message counts"
                );
            }
            let mut missing_field = first.clone();
            let mut report: JsonValue =
                serde_json::from_str(missing_field.verification_report.as_ref().unwrap()).unwrap();
            report["comparison"]
                .as_object_mut()
                .unwrap()
                .remove("exact_remainder");
            replace_report(&mut missing_field, report);
            assert!(!qualifying_subruns(mode, &[missing_field, second]));
        }
        let nonzero_verify = comparison_attempt("verify", 17);
        assert!(!qualifying_subruns("verify", &[nonzero_verify]));
    }

    fn history_row(
        mode: &str,
        outcome: &str,
        attempt: u64,
        inner: Vec<AttemptResult>,
    ) -> CellResult {
        let mut row: CellResult = serde_json::from_value(json!({
            "schema":4,"run_id":"sample","hermit_sha":"a","source_tree_dirty":false,
            "test":"fixture/cell","category":"fixture","lane":"portable","mode":mode,
            "backend":"ptrace","classification":"required","outcome":outcome,"attempt":attempt,
            "argv":["fixture"],"guest_argv":["fixture"],"env":{},"cwd":"/","shell_command":"fixture",
            "attempts":inner,"artifact_dir":format!("/retained/{attempt}")
        })).unwrap();
        row.result = match outcome {
            "PASS" => Some(ObservedResult::Pass),
            "FAIL" => Some(ObservedResult::CrashError),
            _ => None,
        };
        row.failure_class = match outcome {
            "PASS" => None,
            "FAIL" => Some(FailureClass::ProductFailure),
            _ => Some(FailureClass::NoResult),
        };
        row
    }

    fn no_result_attempt(kind: &str, error: Option<&str>) -> AttemptResult {
        let rejected = kind == "first_run_rejected";
        let mut attempt = fixture_attempt(if rejected { "FAIL" } else { "ERROR" }, 125);
        attempt.error_kind = error.map(str::to_owned);
        let mut report = serde_json::to_value(VerificationReport::no_result()).unwrap();
        if rejected {
            report["no_result_reason"] = json!({"kind":kind,"exit_code":17,"signal":null,
                "stdout_bytes":0,"stderr_bytes":0});
            report["guest_exit_code"] = json!(17);
        }
        replace_report(&mut attempt, report);
        attempt
    }

    #[test]
    fn inner_adverse_categories_preserve_outer_failures_and_successful_declared_subruns() {
        let pass = comparison_attempt("chaos", 17);
        let mut declared = pass.clone();
        declared.index = "2".into();
        let row = history_row("chaos", "FAIL", 1, vec![pass.clone(), declared]);
        let neutral = inner_pressure_history(&[row]).unwrap();
        assert!(neutral.is_empty());
        assert_eq!(
            fold_pressure_history(RepetitionClassification::ProductFailure, &neutral),
            RepetitionClassification::ProductFailure
        );
        let mut diverged = comparison_attempt("verify", 0);
        diverged.outcome = "FAIL".into();
        diverged.status = Some(1);
        let mut report: JsonValue =
            serde_json::from_str(diverged.verification_report.as_ref().unwrap()).unwrap();
        report["verdict"] = json!("diverged");
        report["verified"] = json!(false);
        report["bitwise_parity"] = json!(false);
        replace_report(&mut diverged, report);
        let rejected = no_result_attempt("first_run_rejected", None);
        for attempt in [diverged, rejected] {
            let row = history_row("verify", "FAIL", 1, vec![attempt]);
            let categories = inner_pressure_history(&[row]).unwrap();
            assert_eq!(
                categories,
                BTreeSet::from([RepetitionClassification::ProductFailure])
            );
            assert_eq!(
                fold_pressure_history(RepetitionClassification::ProductFailure, &categories),
                RepetitionClassification::ProductFailure
            );
        }
        for (error, expected) in [
            (
                "guest-launch-refused",
                RepetitionClassification::PrerequisiteFailure,
            ),
            (
                "backend-unavailable",
                RepetitionClassification::PrerequisiteFailure,
            ),
            (
                "infrastructure",
                RepetitionClassification::InfrastructureFailure,
            ),
            (
                "result-publication",
                RepetitionClassification::InfrastructureFailure,
            ),
            (
                "incomplete-verification-evidence",
                RepetitionClassification::NoResult,
            ),
            (
                "invalid-backend-evidence",
                RepetitionClassification::NoResult,
            ),
            ("unknown-error", RepetitionClassification::NoResult),
        ] {
            let mut adverse = no_result_attempt("not_run", Some(error));
            adverse.index = "2".into();
            let row = history_row("chaos", "FAIL", 1, vec![pass.clone(), adverse]);
            let categories = inner_pressure_history(&[row]).unwrap();
            assert_eq!(categories, BTreeSet::from([expected]), "{error}");
            assert_eq!(
                fold_pressure_history(RepetitionClassification::ProductFailure, &categories),
                RepetitionClassification::Mixed,
                "{error}"
            );
            assert_eq!(
                fold_pressure_history(expected, &categories),
                expected,
                "{error}"
            );
        }
        let mut noncanonical = pass.clone();
        let mut report: JsonValue =
            serde_json::from_str(noncanonical.verification_report.as_ref().unwrap()).unwrap();
        report["comparison"]["strictness"] = json!("stripped");
        replace_report(&mut noncanonical, report);
        let categories =
            inner_pressure_history(&[history_row("chaos", "FAIL", 1, vec![noncanonical])]).unwrap();
        assert_eq!(
            categories,
            BTreeSet::from([RepetitionClassification::NoResult])
        );
        assert_eq!(
            fold_pressure_history(RepetitionClassification::ProductFailure, &categories),
            RepetitionClassification::Mixed
        );
        let mut infrastructure = no_result_attempt("not_run", Some("infrastructure"));
        let mut report = serde_json::to_value(VerificationReport::no_result()).unwrap();
        report["verdict"] = json!("infrastructure_error");
        report["no_result_reason"] = JsonValue::Null;
        report["infrastructure_error"] = json!({"kind":"skid_overshoot","count":1});
        replace_report(&mut infrastructure, report.clone());
        let retained = retained_pressure_attempt("verify", &infrastructure).unwrap();
        assert_eq!(
            inner_pressure_category(&retained),
            Some(RepetitionClassification::InfrastructureFailure)
        );
        for (field, value) in [
            ("verified", json!(true)),
            ("bitwise_parity", json!(true)),
            (
                "infrastructure_error",
                json!({"kind":"skid_overshoot","count":0}),
            ),
        ] {
            let mut bad = infrastructure.clone();
            let mut changed = report.clone();
            changed[field] = value;
            replace_report(&mut bad, changed);
            assert!(
                retained_pressure_attempt("verify", &bad).is_err(),
                "{field}"
            );
        }
        let mut missing_timeout = fixture_attempt("ERROR", 124);
        missing_timeout.timed_out = true;
        missing_timeout.error_kind = Some("wall-timeout".into());
        assert_eq!(
            inner_pressure_category(
                &retained_pressure_attempt("verify", &missing_timeout).unwrap()
            ),
            Some(RepetitionClassification::NoResult)
        );
        let mut bad_timeout = missing_timeout.clone();
        bad_timeout.error_kind = None;
        assert!(retained_pressure_attempt("verify", &bad_timeout).is_err());
        for cause in [
            "cpu-timeout",
            "wall-timeout",
            "incomplete-verification-evidence",
        ] {
            let mut prelaunch = no_result_attempt("not_run", Some(cause));
            prelaunch.timed_out = true;
            prelaunch.status = None;
            assert_eq!(
                inner_pressure_category(&retained_pressure_attempt("verify", &prelaunch).unwrap()),
                Some(RepetitionClassification::NoResult),
                "{cause}"
            );
            let mut contradiction = prelaunch.clone();
            contradiction.timed_out = false;
            assert!(
                retained_pressure_attempt("verify", &contradiction).is_err(),
                "{cause}"
            );
        }
        let rejected = no_result_attempt("first_run_rejected", None);
        for (field, value) in [
            ("exit_code", json!("17")),
            ("exit_code", json!(18)),
            ("signal", json!("9")),
            ("signal", json!(9)),
        ] {
            let mut bad = rejected.clone();
            let mut report: JsonValue =
                serde_json::from_str(bad.verification_report.as_ref().unwrap()).unwrap();
            report["no_result_reason"][field] = value;
            replace_report(&mut bad, report);
            assert!(
                retained_pressure_attempt("verify", &bad).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn missing_or_malformed_inner_history_is_incomplete_without_changing_terminal_passes() {
        let valid = history_row("verify", "PASS", 1, vec![comparison_attempt("verify", 0)]);
        assert!(
            inner_pressure_history(std::slice::from_ref(&valid))
                .unwrap()
                .is_empty()
        );
        assert!(repetition_qualifies_for_promotion(
            "pass",
            std::slice::from_ref(&valid)
        ));
        let mut dirty = valid.clone();
        dirty.source_tree_dirty = true;
        assert!(!repetition_qualifies_for_promotion("pass", &[dirty]));
        let mut counts = RepeatedOutcomeCounts {
            expected_repetitions: 10,
            observed_repetitions: 10,
            terminal_passes: 10,
            clean_passes: 10,
            qualifying_passes: 10,
            ..RepeatedOutcomeCounts::default()
        };
        assert_eq!(
            classify_pressure_sample(counts),
            PressureSampleClassification::PromotionCandidate
        );
        for edit in [
            (|row: &mut CellResult| row.attempts.clear()) as fn(&mut CellResult),
            |row| row.attempts.push(row.attempts[0].clone()),
            |row| row.attempts[0].index.clear(),
            |row| row.attempts[0].verification_report = None,
            |row| row.attempts[0].verification_report_sha256 = Some("0".repeat(64)),
            |row| row.attempts[0].status = None,
        ] {
            let mut broken = valid.clone();
            edit(&mut broken);
            assert!(inner_pressure_history(&[broken.clone()]).is_err());
            assert_eq!(broken.outcome, "PASS");
            assert_eq!(broken.result, Some(ObservedResult::Pass));
            counts.unknown_history_repetitions = 1;
            counts.qualifying_passes = 9;
            assert_eq!(
                classify_pressure_sample(counts),
                PressureSampleClassification::Incomplete
            );
        }
        let mut noncanonical = valid.clone();
        let mut report: JsonValue = serde_json::from_str(
            noncanonical.attempts[0]
                .verification_report
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        report["comparison"]["compare_io_buffers"] = json!(false);
        replace_report(&mut noncanonical.attempts[0], report);
        assert_eq!(
            inner_pressure_history(&[noncanonical]).unwrap(),
            BTreeSet::from([RepetitionClassification::NoResult])
        );
        counts.unknown_history_repetitions = 0;
        assert_eq!(
            classify_pressure_sample(counts),
            PressureSampleClassification::Intermittent
        );
    }

    #[test]
    fn outer_retry_selection_refuses_post_pass_and_keeps_failure_before_error() {
        let fail = history_row("naked", "FAIL", 1, vec![fixture_attempt("FAIL", 1)]);
        let mut error = history_row("naked", "ERROR", 2, vec![fixture_attempt("ERROR", 125)]);
        error.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
        error.attempts[0].error_kind = Some("infrastructure".into());
        let failure_history = vec![fail.clone(), error.clone()];
        assert_eq!(
            cell_result_after_retries(&failure_history).unwrap().outcome,
            "FAIL"
        );
        assert_eq!(
            inner_pressure_history(&failure_history).unwrap(),
            BTreeSet::from([
                RepetitionClassification::ProductFailure,
                RepetitionClassification::InfrastructureFailure
            ])
        );
        assert_eq!(
            classify_nonpassing_repetition(
                "crash-error",
                &failure_history,
                true,
                true,
                false,
                false,
                false,
                false
            ),
            RepetitionClassification::Mixed
        );
        let mut pass = history_row("naked", "PASS", 2, vec![fixture_attempt("PASS", 0)]);
        error.attempt = 1;
        let recovered = vec![error.clone(), pass.clone()];
        assert_eq!(
            cell_result_after_retries(&recovered).unwrap().outcome,
            "PASS"
        );
        assert!(!repetition_qualifies_for_promotion("pass", &recovered));
        pass.attempt = 1;
        error.attempt = 2;
        for malformed in [
            vec![pass.clone(), error.clone()],
            vec![fail.clone(), fail.clone()],
            vec![fail.clone(), {
                let mut gap = error.clone();
                gap.attempt = 3;
                gap
            }],
            vec![fail.clone(), error.clone(), {
                let mut third = error.clone();
                third.attempt = 3;
                third
            }],
            vec![
                {
                    let mut host = pass.clone();
                    host.outcome = "HOST-INAPPLICABLE".into();
                    host
                },
                error,
            ],
        ] {
            assert!(cell_result_after_retries(&malformed).is_err());
            assert!(inner_pressure_history(&malformed).is_err());
        }
    }

    #[test]
    fn summary_requires_retained_canonical_captures_and_golden_before_confirmed_failure() {
        let root = Path::new(file!())
            .canonicalize()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let checked = check_scorecard(&root).unwrap();
        let available = pressure_cells(&root, &CellSelection::default()).unwrap();
        let selected = available
            .selected
            .iter()
            .find(|cell| cell.id.mode == "verify" && cell.id.backend == "ptrace")
            .expect("fixture needs a selected ptrace verify cell");
        let results = env::temp_dir().join(format!(
            "hermit-pressure-summary-artifacts-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&results).unwrap();
        let cleanup = SelfTestDirectory::new(results.clone());
        let selection = CellSelection {
            test: Some(selected.id.test.clone()),
            mode: Some("verify".into()),
            backend: Some("ptrace".into()),
            repetitions: Some(PROMOTION_REPETITIONS),
            run_id_prefix: Some("retained-artifacts".into()),
            run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
            ..CellSelection::default()
        };
        let (mut metadata, _) = write_plan_after_scorecard_check(
            &checked,
            &results,
            &results.join("dag.json"),
            &selection,
        )
        .unwrap();
        metadata.source_tree_dirty = false;
        fs::write(
            results.join("run.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        let mut evidence = BTreeMap::new();
        let mut first_paths = None;
        let mut inner = comparison_attempt("verify", 0);
        inner.outcome = "FAIL".into();
        inner.status = Some(1);
        inner.shell_command = literal_shell_command(&inner.cwd, &inner.env, &inner.argv);
        let mut report: JsonValue =
            serde_json::from_str(inner.verification_report.as_ref().unwrap()).unwrap();
        report["verdict"] = json!("diverged");
        report["verified"] = json!(false);
        report["bitwise_parity"] = json!(false);
        replace_report(&mut inner, report);
        for repetition in 1..=PROMOTION_REPETITIONS {
            let slug = cell_run_slug(&selected.id, Some(repetition));
            let run_id = cell_evidence_run_id(
                &selected.id,
                Some(repetition),
                metadata.run_id_prefix.as_deref(),
            );
            let cell_dir = results.join("cells").join(&slug);
            fs::create_dir_all(&cell_dir).unwrap();
            fs::write(cell_dir.join("harness-status"), "1\n").unwrap();
            let artifact = results.join("runs").join(&run_id).join("attempt-1");
            let logs = artifact.join("verify-logs/verify-1");
            fs::create_dir_all(&logs).unwrap();
            let run1 = logs.join("run1_log_fixture.log");
            let run2 = logs.join("run2_log_fixture.log");
            let golden = logs.join("normalized-ptrace-golden.log");
            fs::write(&run1, "INFO first\n").unwrap();
            fs::write(&run2, "INFO second\n").unwrap();
            fs::write(&golden, "INFO normalized\n").unwrap();
            fs::write(logs.join("normalized-ptrace-golden.status"), "0\n").unwrap();
            fs::write(
                verification_report_path(&artifact),
                inner.verification_report.as_ref().unwrap(),
            )
            .unwrap();
            if first_paths.is_none() {
                first_paths = Some((run1, golden));
            }
            let mut row = history_row("verify", "FAIL", 1, vec![inner.clone()]);
            row.run_id = run_id;
            row.run_index = Some(repetition as u64);
            row.hermit_sha = metadata.hermit_sha.clone();
            row.test = selected.id.test.clone();
            row.category = selected.id.category.clone();
            row.lane = selected.id.lane.clone();
            row.classification = if selected.enabled {
                "required"
            } else {
                "disabled"
            }
            .into();
            row.result = Some(ObservedResult::DeterminismFailure);
            row.failure_class = Some(FailureClass::ProductFailure);
            row.argv = inner.argv.clone();
            row.guest_argv = inner.guest_argv.clone();
            row.env = inner.env.clone();
            row.cwd = inner.cwd.clone();
            row.shell_command = inner.shell_command.clone();
            row.timeout_seconds = 57;
            row.execution_cpu_timeout_seconds = Some(22);
            row.execution_wall_timeout_seconds = Some(57);
            row.artifact_dir = artifact.to_string_lossy().into_owned();
            fs::write(
                cell_dir.join("results.jsonl"),
                format!("{}\n", serde_json::to_string(&row).unwrap()),
            )
            .unwrap();
            evidence.insert(
                format!("cell.{slug}"),
                RunnerEvidence {
                    seen: true,
                    ok: false,
                    ..RunnerEvidence::default()
                },
            );
        }
        let read = || -> JsonValue {
            serde_json::from_slice(&fs::read(results.join("summary.json")).unwrap()).unwrap()
        };
        summarize(&root, &results, false, Some(&evidence), true).unwrap();
        let complete = read();
        assert_eq!(
            complete["repeated_cells"][0]["terminal_product_failures"],
            10
        );
        assert_eq!(
            complete["repeated_cells"][0]["unknown_history_repetitions"],
            0
        );
        assert_eq!(
            complete["repeated_cells"][0]["classification"],
            "confirmed-failing"
        );
        let (run1, golden) = first_paths.unwrap();
        for missing in [&run1, &golden] {
            let saved = fs::read(missing).unwrap();
            fs::remove_file(missing).unwrap();
            let error = summarize(&root, &results, false, Some(&evidence), true).unwrap_err();
            assert!(error.contains("no trustworthy result"), "{error}");
            let incomplete = read();
            assert_eq!(
                incomplete["repeated_cells"][0]["classification"],
                "incomplete",
                "{}",
                missing.display()
            );
            assert_eq!(
                incomplete["repeated_cells"][0]["unknown_history_repetitions"],
                1
            );
            assert_eq!(
                incomplete["repeated_cells"][0]["terminal_product_failures"],
                9
            );
            assert_eq!(incomplete["rows"][0]["result"], "infrastructure-error");
            fs::write(missing, saved).unwrap();
        }
        summarize(&root, &results, false, Some(&evidence), true).unwrap();
        assert_eq!(read(), complete);
        let first_slug = cell_run_slug(&selected.id, Some(1));
        let first_row_path = results.join("cells").join(first_slug).join("results.jsonl");
        let original_row = fs::read(&first_row_path).unwrap();
        let mut rejected_row: CellResult = serde_json::from_slice(&original_row).unwrap();
        let mut rejected = no_result_attempt("first_run_rejected", None);
        rejected.argv = inner.argv.clone();
        rejected.guest_argv = inner.guest_argv.clone();
        rejected.env = inner.env.clone();
        rejected.cwd = inner.cwd.clone();
        rejected.shell_command = inner.shell_command.clone();
        rejected_row.attempts = vec![rejected.clone()];
        rejected_row.result = Some(ObservedResult::CrashError);
        let retained_report_path = verification_report_path(Path::new(&rejected_row.artifact_dir));
        let original_report = fs::read(&retained_report_path).unwrap();
        fs::write(
            &retained_report_path,
            rejected.verification_report.as_ref().unwrap(),
        )
        .unwrap();
        fs::write(&first_row_path, serde_json::to_vec(&rejected_row).unwrap()).unwrap();
        let error = summarize(&root, &results, false, Some(&evidence), true).unwrap_err();
        assert!(error.contains("no trustworthy result"));
        let rejected_summary = read();
        assert_eq!(
            rejected_summary["repeated_cells"][0]["classification"],
            "confirmed-failing"
        );
        assert_eq!(
            rejected_summary["repeated_cells"][0]["terminal_product_failures"],
            10
        );
        assert_eq!(
            rejected_summary["repeated_cells"][0]["unknown_history_repetitions"],
            0
        );
        assert_eq!(
            rejected_summary["rows"][0]["result"],
            "infrastructure-error"
        );
        // A different valid NoResult stamp cannot explain this row's selected artifact error.
        fs::write(
            &retained_report_path,
            serde_json::to_vec(&VerificationReport::no_result()).unwrap(),
        )
        .unwrap();
        assert!(summarize(&root, &results, false, Some(&evidence), true).is_err());
        let mismatched = read();
        assert_eq!(
            mismatched["repeated_cells"][0]["classification"],
            "incomplete"
        );
        assert_eq!(
            mismatched["repeated_cells"][0]["unknown_history_repetitions"],
            1
        );
        fs::write(&retained_report_path, original_report).unwrap();
        fs::write(&first_row_path, original_row).unwrap();

        cleanup.remove().unwrap();
    }

    #[test]
    fn host_prerequisite_requires_exact_cell_and_complete_nonproduct_counts() {
        let path = env::temp_dir().join(format!(
            "hermit-pressure-self-test-sample-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let guard = SelfTestDirectory::new(path.clone());
        let cell = CellId {
            lane: "portable".into(),
            category: "sample".into(),
            test: "sample/cell".into(),
            mode: "naked".into(),
            backend: "native".into(),
        };
        let good = json!({"schema":1,"cells":1,"passed":0,"failed":0,"errors":0,
            "host_inapplicable":1,"host_inapplicable_cells":[{"test":cell.test,
            "mode":cell.mode,"backend":null,"reason":"required capability unavailable"}]});
        let write = |value: &JsonValue| {
            fs::write(
                path.join("summary.json"),
                serde_json::to_vec(value).unwrap(),
            )
            .unwrap()
        };
        write(&good);
        assert!(retained_host_inapplicable(&path, &cell).unwrap());
        for (field, value) in [
            ("schema", 2),
            ("cells", 2),
            ("passed", 1),
            ("failed", 1),
            ("errors", 1),
            ("host_inapplicable", 2),
        ] {
            let mut changed = good.clone();
            changed[field] = json!(value);
            write(&changed);
            assert!(retained_host_inapplicable(&path, &cell).is_err(), "{field}");
        }
        for field in ["test", "mode", "backend", "reason"] {
            let mut changed = good.clone();
            changed["host_inapplicable_cells"][0][field] = json!("");
            write(&changed);
            assert!(retained_host_inapplicable(&path, &cell).is_err(), "{field}");
        }
        let mut duplicate = good.clone();
        duplicate["host_inapplicable_cells"]
            .as_array_mut()
            .unwrap()
            .push(good["host_inapplicable_cells"][0].clone());
        write(&duplicate);
        assert!(retained_host_inapplicable(&path, &cell).is_err());
        fs::write(path.join("summary.json"), "{").unwrap();
        assert!(retained_host_inapplicable(&path, &cell).is_err());
        guard.remove().unwrap();
    }
}

#[cfg(test)]
mod pressure_planning_tests {
    use super::*;

    fn memory_fixture() -> DagConfig {
        let gib = 1024_i64 * 1024 * 1024;
        let step = |group: &str, job: &str, cap: i64, deps: Vec<&str>, resources: JsonValue| json!({
            "group": group, "job": job, "cmd": "true", "timeout": 10, "cpu_timeout": 20,
            "deps": deps, "hint": {"hard_mem_max_bytes": cap * gib, "resources": resources},
        });
        let config = json!({
            "resource_caps": {"cargo_writer": 1, "manifest_guest": 2, "kvm_guest": 1},
            "steps": [
                step("pre", "submodules", 2, vec![], json!({})),
                step("setup", "manifest_plan", 1, vec!["pre.submodules"], json!({})),
                step("gate", "manifest", 5, vec!["setup.manifest_plan"], json!({})),
                step("build", "workspace", 7, vec!["gate.manifest"], json!({})),
                step("build", "liteinst_runtime_release", 6, vec!["build.workspace"], json!({})),
                step("prepare", "later-test", 3, vec!["build.workspace"], json!({"cargo_writer": 1})),
                step("cell", "already-prepared-kvm", 16, vec!["build.workspace"], json!({"manifest_guest": 1, "kvm_guest": 1})),
                step("cell", "already-prepared-native", 3, vec!["build.workspace"], json!({"manifest_guest": 1})),
                step("pressure", "summarize", 1, vec!["build.liteinst_runtime_release", "prepare.later-test", "cell.already-prepared-kvm", "cell.already-prepared-native"], json!({})),
            ],
        });
        dag_from_json(&config.to_string()).expect("complete memory fixture parses")
    }

    #[test]
    fn memory_phases_account_for_prerequisites_and_simultaneous_support() {
        let mut dag = memory_fixture();
        let gib = 1024_i64 * 1024 * 1024;
        // Four workers can run 16+3 GiB cells plus the 6+3 GiB support nodes.
        // The separate 1 GiB reserve makes 29 GiB, above the 15 GiB initial sum.
        assert_eq!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap(), 29 * gib);
        let native = dag.steps.iter_mut().find(|step| step.job == "already-prepared-native").unwrap();
        native.hint.hard_mem_max_bytes = Some(16 * gib);
        // Privileged non-KVM cells may tie the KVM cap without changing the proof.
        assert_eq!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap(), 42 * gib);
        let gate = dag.steps.iter_mut().find(|step| step.tag() == "gate.manifest").unwrap();
        gate.hint.hard_mem_max_bytes = Some(50 * gib);
        // All four early caps, including the gate, are charged: 2+1+50+7+1.
        assert_eq!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap(), 61 * gib);
    }

    #[test]
    fn memory_phases_refuse_unordered_unaccounted_or_missing_nodes() {
        for tag in ["prepare.later-test", "cell.already-prepared-native", "build.liteinst_runtime_release"] {
            let mut dag = memory_fixture();
            dag.steps.iter_mut().find(|step| step.tag() == tag).unwrap().deps.clear();
            let error = declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err();
            assert!(error.contains("does not wait for initial node"), "{tag}: {error}");
        }
        let mut dag = memory_fixture();
        dag.steps.last_mut().unwrap().deps.clear();
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("can overlap unfinished node"));
        let mut dag = memory_fixture();
        dag.steps[0].group = "unknown".into();
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("unaccounted step"));
        let mut dag = memory_fixture();
        dag.steps[0].hint.hard_mem_max_bytes = None;
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("no positive hard memory cap"));
        let mut dag = memory_fixture();
        dag.steps.push(dag.steps[0].clone());
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("duplicate step"));
        let mut dag = memory_fixture();
        dag.steps[0].deps = vec!["missing.producer".into()];
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("absent dependency"));
        let mut dag = memory_fixture();
        dag.steps[0].deps = vec!["setup.manifest_plan".into()];
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("dependency cycle"));
    }

    #[test]
    fn memory_phases_refuse_unbounded_resources_or_invalid_kvm_ordering() {
        for resource in ["cargo_writer", "manifest_guest", "kvm_guest"] {
            let mut dag = memory_fixture();
            if resource == "cargo_writer" {
                dag.resource_caps.insert(resource.into(), 2);
            } else {
                let cell = dag.steps.iter_mut().find(|step| step.job == "already-prepared-kvm").unwrap();
                cell.hint.resources.insert(resource.into(), 2);
            }
            assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).is_err(), "{resource}");
        }
        let mut dag = memory_fixture();
        dag.steps.iter_mut().find(|step| step.job == "later-test").unwrap().hint.resources.clear();
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("single cargo_writer"));
        let mut dag = memory_fixture();
        dag.steps.iter_mut().find(|step| step.job == "already-prepared-native").unwrap().hint.hard_mem_max_bytes = Some(17 * 1024 * 1024 * 1024);
        assert!(declared_memory_at_manifest_guest_cap(&dag, 4, 2, 1).unwrap_err().contains("KVM-first memory admission"));
    }

    #[test]
    fn direct_and_retained_guest_widths_must_be_positive() {
        for invalid in [0, -1] {
            for selection in [
                CellSelection { jobs: Some(invalid), ..CellSelection::default() },
                CellSelection { manifest_guest_cap: Some(invalid), ..CellSelection::default() },
                CellSelection { kvm_guest_cap: Some(invalid), ..CellSelection::default() },
            ] {
                assert!(validate_selection_shape(&selection).unwrap_err().contains("must be positive"));
            }
        }
        let cell: TrackedCell = serde_json::from_value(json!({
            "backend": "ptrace", "category": "applications", "lane": "portable",
            "mode": "verify", "test": "applications/example-timed-progress-bar",
            "enabled": true, "status": "red",
        })).unwrap();
        let mut selection = CellSelection {
            repetitions: Some(1), jobs: Some(4), kvm_guest_cap: Some(1),
            ..CellSelection::default()
        };
        validate_selection_shape(&selection).unwrap();
        let error = validate_guest_caps_against_selected_demand(std::slice::from_ref(&cell), &selection).unwrap_err();
        assert!(error.contains("effective KVM demand 0"), "{error}");
        assert!(error.contains("omit --kvm-guest-cap"), "{error}");
        selection.kvm_guest_cap = None;
        validate_selection_shape(&selection).unwrap();
        validate_guest_caps_against_selected_demand(std::slice::from_ref(&cell), &selection).unwrap();
        assert!(USAGE.contains("identities of enabled executable red cells"));
    }
}
