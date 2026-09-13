use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;

use dagrun::TestResult;
use dagrun::TestResults;
use hermit_manifest_plan::cli_help::is_help_flag;
use hermit_manifest_plan::runner::CellResult;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::MAX_ATTEMPTS_PER_CELL;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::append_result;
use hermit_manifest_plan::runner::cell_result_after_retries;
use hermit_manifest_plan::runner::cell_result_and_attempts_after_retries;
use hermit_manifest_plan::runner::checked_add_cpu_usage;
use hermit_manifest_plan::runner::host_inapplicable_result;
use hermit_manifest_plan::runner::infrastructure_error_result;
use hermit_manifest_plan::runner::prepare_result_path;
use hermit_manifest_plan::runner::requires_capability;
use hermit_manifest_plan::runner::run_cell;
use hermit_manifest_plan::runner::write_junit;
use hermit_manifest_plan::stress_series::HostCapabilities;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapability;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapabilityVerdict;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

const EXPECTED_PLAN_SCHEMA: u64 = 1;
const VALIDATE_AUDIT_JOBS: usize = 2;
const PREBUILT_RUST_SCRIPTS_REQUIRED: &str = "HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED";
const DEFAULT_BUILD_JOBS: usize = 16;

const HELP: &str = "\
Usage: test-harness <COMMAND> [OPTIONS]

Validate, inspect, build, and run the centralized Hermit end-to-end manifests.

Commands:
  validate                         Validate manifests and their CI/DAG correspondence
  plan                             List selected required cells
  expected-plan                    Print the expected CI plan as JSON
  audit-gaps                       List selected disabled cells
  audit-inventory                  Validate the manifest-owned test inventory
  audit-test-binary-registration   Validate test binary registration
  audit-test-footprints            Check the generated test footprints
  audit-ci                         Audit DAG, budget, and expected-plan correspondence
  build                            Prepare selected test programs
  audit-compile                    Compile selected C test programs
  run                              Execute selected cells

Selection options:
  --lane <portable|privileged>
  --category <CATEGORY>
  --test <ID>
  --mode <verify|chaos|replay|naked|custom>
  --backend <ptrace|dbt|kvm|sabre|liteinst>
  --ci-only                        Select required CI cells
  --include-occasional             Include occasional cells
  --include-manual                 Include manual cells; requires exact test and mode
  --probe-disabled                 Run one exact disabled cell

Execution and output options:
  --prebuilt                       Reuse prepared test programs (run only)
  --allow-empty                    Permit an empty explicit CI selection
  --results <PATH>                 Write JSONL cell results to PATH
  --junit <PATH>                   Write JUnit output to PATH
  --format <text|json>             Plan output format (default: text)
  --jobs <N>                       Prepare/run at most N tests/cells concurrently
  -h, --help                       Print this help";

const PUBLIC_EXECUTION_ENVIRONMENT: &str =
    "  E2E_RESULT_ROOT=<PATH>                 Result root (default: ignored/e2e)
  E2E_BUILD_ROOT=<PATH>                  Prepared-program build root
  E2E_RUN_ID=<ID>                        Run identifier and result subdirectory
  E2E_RUN_INDEX=<N>                      Non-negative run index recorded in results
  E2E_MACHINE_SHORTNAME=<NAME>           Machine name recorded in results
  E2E_KERNEL_VERSION=<VERSION>           Kernel version recorded in results
  HERMIT_BIN=<PATH>                      Hermit executable (default: target/debug/hermit)
  HERMIT_E2E_EMPTY_WORKDIR=/test         Use the isolated /test working directory
  E2E_KEEP_VERIFY_LOGS=1                 Retain successful verification logs
  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=<N> Positive finite CPU-time multiplier
  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=<N> Positive finite wall-time multiplier";

const FILTER_OPTIONS: &str = "  --lane <portable|privileged>
  --category <CATEGORY>
  --test <ID>
  --mode <verify|chaos|replay|naked|custom>
  --backend <ptrace|dbt|kvm|sabre|liteinst>";

const AMBIENT_PREPARATION_ENVIRONMENT: &str =
    "  HOME=<PATH>                            Base for default Rust toolchain homes
  RUSTUP_HOME=<PATH>                     Rustup home preserved during preparation only
  CARGO_HOME=<PATH>                      Cargo home preserved during preparation only";

#[derive(Clone, Copy)]
enum CommandEnvironment {
    None,
    Execution,
    Run,
}

fn print_command_help(command: &str) -> bool {
    let (summary, filter_options, options, environment) = match command {
        "validate" => (
            "Validate manifests and their CI/DAG correspondence.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "plan" => (
            "List selected required cells.",
            true,
            "  --include-occasional             Include occasional cells\n  \
             --include-manual                 Include manual cells; requires exact test and mode\n  \
             --format <text|json>             Output format (default: text)",
            CommandEnvironment::None,
        ),
        "expected-plan" => (
            "Print the expected CI plan as JSON.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-gaps" => (
            "List selected disabled cells.",
            true,
            "  --include-occasional             Include occasional cells\n  \
             --format <text|json>             Output format (default: text)",
            CommandEnvironment::None,
        ),
        "audit-inventory" => (
            "Validate the manifest-owned test inventory.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-test-binary-registration" => (
            "Validate test binary registration.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-test-footprints" => (
            "Check the generated test footprints.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-ci" => (
            "Audit DAG, budget, and expected-plan correspondence.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "build" => (
            "Prepare selected test programs.",
            true,
            "  --ci-only                        Select required CI cells\n  \
             --include-occasional             Include occasional cells\n  \
             --include-manual                 Include manual cells; requires exact test and mode\n  \
             --allow-empty                    Permit an empty CI selection; requires --ci-only and lane/category",
            CommandEnvironment::Execution,
        ),
        "audit-compile" => (
            "Compile selected C test programs.",
            false,
            "  --lane <portable|privileged>\n  --category <CATEGORY>\n  --test <ID>",
            CommandEnvironment::Execution,
        ),
        "run" => (
            "Execute selected cells.",
            true,
            "  --ci-only                        Select required CI cells\n  \
             --include-occasional             Include occasional cells\n  \
             --include-manual                 Include manual cells; requires exact test and mode\n  \
             --probe-disabled                 Run one disabled cell; requires exact test/mode/backend\n  \
             --prebuilt                       Reuse prepared test programs\n  \
             --allow-empty                    Permit an empty CI selection; requires --ci-only and category\n  \
             --results <PATH>                 Write JSONL cell results to PATH\n  \
             --junit <PATH>                   Write JUnit output to PATH\n  \
             --jobs <N>                       Run at most N cells concurrently",
            CommandEnvironment::Run,
        ),
        _ => return false,
    };
    let options_marker = if filter_options || !options.is_empty() {
        " [OPTIONS]"
    } else {
        ""
    };
    println!("Usage: test-harness {command}{options_marker}\n\n{summary}\n\nOptions:");
    if filter_options {
        println!("{FILTER_OPTIONS}");
    }
    if !options.is_empty() {
        println!("{options}");
    }
    println!("  -h, --help                       Print this help");
    if matches!(
        environment,
        CommandEnvironment::Execution | CommandEnvironment::Run
    ) {
        println!("\nEnvironment:\n{PUBLIC_EXECUTION_ENVIRONMENT}");
        println!("\nAmbient fixture-preparation environment:\n{AMBIENT_PREPARATION_ENVIRONMENT}");
    }
    if matches!(environment, CommandEnvironment::Run) {
        println!(
            "\nInternal runner protocol:\n  \
             DAGRUN_TEST_COUNTS_PATH=<PATH>       Write schema-2 test counts for dagrun"
        );
    }
    true
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("test-harness: {message}");
    std::process::exit(2);
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[derive(Default)]
struct Args {
    selection: Selection,
    prebuilt: bool,
    allow_empty: bool,
    ci_only: bool,
    probe_disabled: bool,
    results: Option<PathBuf>,
    junit: Option<PathBuf>,
    format: String,
    jobs: Option<usize>,
}

/// Take a single-valued selection flag, refusing a second occurrence.
///
/// ⚠️ LAST-VALUE-WINS SILENTLY UNDID THIS FILE'S OWN GUARD, WHICH IS WHY THIS EXISTS.
/// `plan` refuses a `--test` naming no known id. With plain assignment a SECOND
/// `--test` overwrote the first, so the unknown one was never looked up at all:
///
/// ```text
/// plan --lane portable --test no-such-test-xyz                             rc=2
/// plan --lane portable --test no-such-test-xyz --test applications/...     rc=0   []
/// plan --lane portable --test applications/... --test no-such-test-xyz     rc=2
/// ```
///
/// Measured 2026-08-26 at `979a50b17a75` by `agent(codex-rev-2686)` and confirmed
/// independently by `agent(hermit-012)` and by me. The asymmetry is the tell: the
/// same two ids in the other order refuse, because only the LAST occurrence is ever
/// examined. A bisection driver reading rc=0 there sees "nothing failed" for a list
/// containing an id that does not exist -- the exact silent green this guard was
/// added to remove, reappearing one layer up in the argument parser.
///
/// `--jobs` already refused a repeat; the selection flags did not. Refusing is right
/// rather than taking the first or the last, because a repeated selector has no
/// defensible meaning: the caller asked for two different things and we cannot serve
/// both from one field.
fn set_once(slot: &mut Option<String>, values: &mut impl Iterator<Item = String>, flag: &str) {
    let value = required_value(values, flag);
    if slot.replace(value).is_some() {
        fail(format!(
            "{flag} may be specified only once; a repeat silently overwrote the first \
             value, so an earlier id was never validated"
        ));
    }
}

fn parse(mut values: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        format: "text".into(),
        ..Args::default()
    };
    while let Some(value) = values.next() {
        match value.as_str() {
            "--lane" => set_once(&mut args.selection.lane, &mut values, "--lane"),
            "--category" => set_once(&mut args.selection.category, &mut values, "--category"),
            "--test" => set_once(&mut args.selection.test, &mut values, "--test"),
            "--mode" => set_once(&mut args.selection.mode, &mut values, "--mode"),
            "--backend" => set_once(&mut args.selection.backend, &mut values, "--backend"),
            "--ci-only" => {
                args.ci_only = true;
                args.selection.population = Some(Population::Required);
            }
            "--include-occasional" => args.selection.include_occasional = true,
            "--include-manual" => args.selection.include_manual = true,
            "--probe-disabled" => {
                args.probe_disabled = true;
                args.selection.population = Some(Population::Disabled);
            }
            "--prebuilt" => args.prebuilt = true,
            "--allow-empty" => args.allow_empty = true,
            "--results" => {
                args.results = Some(PathBuf::from(required_value(&mut values, "--results")))
            }
            "--junit" => args.junit = Some(PathBuf::from(required_value(&mut values, "--junit"))),
            "--format" => args.format = required_value(&mut values, "--format"),
            "--jobs" => {
                let value = required_value(&mut values, "--jobs");
                let jobs = value
                    .parse::<usize>()
                    .ok()
                    .filter(|jobs| *jobs > 0)
                    .unwrap_or_else(|| fail("--jobs requires a positive integer"));
                if args.jobs.replace(jobs).is_some() {
                    fail("--jobs may be specified only once");
                }
            }
            other => fail(format!("unknown option {other}")),
        }
    }
    args
}

fn structured_test_results(histories: &[Vec<CellResult>]) -> Result<TestResults, String> {
    let rows = histories
        .iter()
        .map(|history| {
            let (result, attempts) = cell_result_and_attempts_after_retries(history)?;
            Ok((result.outcome != "HOST-INAPPLICABLE").then(|| {
                (
                    format!(
                        "{} [{}/{}]",
                        result.test,
                        result.backend.as_deref().unwrap_or("native"),
                        result.mode
                    ),
                    result.outcome == "PASS",
                    attempts,
                )
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    structured_test_results_from_rows(rows.into_iter().flatten())
}

fn structured_test_results_from_rows(
    rows: impl IntoIterator<Item = (String, bool, u64)>,
) -> Result<TestResults, String> {
    let rows = rows
        .into_iter()
        .map(|(id, passed, attempts)| TestResult::new(id, passed, attempts))
        .collect::<Result<Vec<_>, _>>()?;
    TestResults::current(
        u64::try_from(rows.len()).map_err(|_| "cell result count does not fit u64")?,
        0,
        rows,
    )
}

fn accumulate_cell_cpu_usage(
    total: &mut Option<u64>,
    measurements: &mut usize,
    outcome: &str,
    usage: Option<u64>,
) {
    if outcome != "HOST-INAPPLICABLE" {
        *measurements += 1;
        *total = checked_add_cpu_usage(*total, usage);
    }
}

fn host_inapplicable_reason(
    requires: &[String],
    verdicts: &HostCapabilities,
) -> Option<(Vec<String>, String)> {
    let mut absent = requires
        .iter()
        .filter_map(|token| requires_capability(token).ok().flatten())
        .filter_map(|capability| {
            verdicts
                .get(&capability)
                .filter(|verdict| !verdict.present)
                .map(|verdict| (capability.value().to_string(), verdict.evidence.clone()))
        })
        .collect::<Vec<_>>();
    absent.sort();
    absent.dedup();
    if absent.is_empty() {
        return None;
    }
    let capabilities = absent
        .iter()
        .map(|(capability, _)| capability.clone())
        .collect::<Vec<_>>();
    let reason = format!(
        "NOT RUN, NOT a pass, no coverage: this machine lacks {}",
        absent
            .iter()
            .map(|(capability, evidence)| format!("{capability} ({evidence})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some((capabilities, reason))
}

fn scheduled_worker_capacity(args: &Args) -> ScheduledWorkerCapacity {
    ScheduledWorkerCapacity::new(args.jobs.unwrap_or(1))
}

fn build_worker_capacity(args: &Args) -> ScheduledWorkerCapacity {
    ScheduledWorkerCapacity::new(args.jobs.unwrap_or(DEFAULT_BUILD_JOBS))
}

fn required_value(values: &mut impl Iterator<Item = String>, option: &str) -> String {
    let value = values
        .next()
        .unwrap_or_else(|| fail(format!("{option} requires a value")));
    if value.trim().is_empty() {
        fail(format!("{option} requires a non-empty value"));
    }
    value
}

fn validate_args(command: &str, args: &Args) {
    if !matches!(args.format.as_str(), "text" | "json") {
        fail(format!("invalid format {}", args.format));
    }
    if args
        .selection
        .lane
        .as_deref()
        .is_some_and(|lane| !matches!(lane, "portable" | "privileged"))
    {
        fail("--lane must be portable or privileged");
    }
    if args
        .selection
        .mode
        .as_deref()
        .is_some_and(|mode| !matches!(mode, "verify" | "chaos" | "replay" | "naked" | "custom"))
    {
        fail("--mode must be verify, chaos, replay, naked, or custom");
    }
    if args
        .selection
        .backend
        .as_deref()
        .is_some_and(|backend| !matches!(backend, "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst"))
    {
        fail("--backend must name a Hermit backend");
    }
    if command == "build" && args.prebuilt {
        fail("build does not accept --prebuilt");
    }
    if !matches!(command, "build" | "run") && args.jobs.is_some() {
        fail("--jobs is accepted by build and run only");
    }
    if args.selection.include_manual
        && (args.selection.test.is_none() || args.selection.mode.is_none())
    {
        fail("--include-manual requires exact --test and --mode filters");
    }
    if args.probe_disabled {
        if command != "run" {
            fail("--probe-disabled is accepted by run only");
        }
        if args.selection.test.is_none()
            || args.selection.mode.is_none()
            || args.selection.backend.is_none()
        {
            fail("--probe-disabled requires exact --test, --mode, and --backend filters");
        }
        if args.selection.include_manual || args.ci_only {
            fail("--probe-disabled is mutually exclusive with --include-manual and --ci-only");
        }
    }
    if args.allow_empty {
        if !args.ci_only {
            fail("--allow-empty requires --ci-only");
        }
        match command {
            "build" if args.selection.lane.is_some() || args.selection.category.is_some() => {}
            "run" if args.selection.category.is_some() => {}
            "build" => fail("build --allow-empty requires an explicit --lane or --category"),
            "run" => fail("run --allow-empty requires an explicit --category"),
            _ => fail("--allow-empty is accepted by build and run only"),
        }
    }
}

fn main() -> ExitCode {
    let values = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(values.as_slice(), [flag] if is_help_flag(flag)) {
        println!("{HELP}\n\nEnvironment:\n{PUBLIC_EXECUTION_ENVIRONMENT}");
        return ExitCode::SUCCESS;
    }
    if let [command, flag] = values.as_slice() {
        if is_help_flag(flag) && print_command_help(command) {
            return ExitCode::SUCCESS;
        }
    }
    let mut values = values.into_iter();
    let command = values
        .next()
        .unwrap_or_else(|| fail("missing command; try `test-harness --help`"));
    let values = values.collect::<Vec<_>>();
    if command == "expected-plan" && !values.is_empty() {
        fail("expected-plan accepts no options");
    }
    let args = parse(values.into_iter());
    validate_args(&command, &args);
    let root = root();
    let manifests = ManifestSet::load(&root).unwrap_or_else(|error| fail(error));
    // One front-door schema/inventory authority governs every command, not
    // only the metadata gate. This prevents a direct/manual run from accepting
    // a recipe that the canonical manifest planner would refuse.
    run_manifest_plan(&root);
    match command.as_str() {
        "validate" => validate(&root, &manifests),
        "plan" => print_plan(&manifests, &args, Population::Required),
        "expected-plan" => print_expected_plan(&root, &manifests),
        "audit-gaps" => print_plan(&manifests, &args, Population::Disabled),
        "audit-inventory" | "audit-test-binary-registration" => ExitCode::SUCCESS,
        "audit-test-footprints" => {
            run_audit(
                &root,
                &root.join("target/debug/generate-test-footprints"),
                &["--check"],
            );
            ExitCode::SUCCESS
        }
        "audit-ci" => {
            audit_dag_correspondence(&root, &manifests).unwrap_or_else(|error| fail(error));
            audit_budget_ordering(&root).unwrap_or_else(|error| fail(error));
            audit_expected_plan(&root, &manifests);
            ExitCode::SUCCESS
        }
        "build" => build(&root, &manifests, &args),
        "audit-compile" => audit_compile(&root, &manifests, &args),
        "run" => run(&root, &manifests, &args),
        other => fail(format!("unknown command {other}")),
    }
}

fn validate(root: &Path, manifests: &ManifestSet) -> ExitCode {
    // Keep the existing Rust manifest-plan front door as the authority for
    // inventory, schema, lane, workflow, and DAG consistency.  The cell
    // runner owns execution; it must not silently narrow `validate` to only
    // the expected-plan comparison during the shell removal.
    audit_dag_correspondence(root, manifests).unwrap_or_else(|error| fail(error));
    audit_budget_ordering(root).unwrap_or_else(|error| fail(error));
    audit_determinism_stress_evidence(root);
    // These self-contained audits read the same checked-out tree but keep all
    // generated state in their own temporary directories. The validation DAG
    // supplies immutable prebuilt rust-script binaries; without that guarantee,
    // retain the old serial order instead of making rust-script compilers contend
    // for their shared cache. Run no more than two at once: two is the largest
    // clean concurrent validation width established on this host, and a wider
    // unmeasured default would turn this speed change into a new concurrency
    // assumption. Capture each child independently and replay it in the original
    // order so diagnostics remain attributable.
    let audit_jobs = if std::env::var(PREBUILT_RUST_SCRIPTS_REQUIRED).as_deref() == Ok("1") {
        VALIDATE_AUDIT_JOBS
    } else {
        1
    };
    run_audits_parallel(
        root,
        &[
            (
                root.join("target/debug/generate-test-footprints"),
                vec!["--check"],
            ),
            (
                root.join("tests/backend-parity/split_asymmetric_pr.py"),
                vec!["--self-test"],
            ),
            (root.join("tests/manifest-cli.rs"), vec!["self-test"]),
            // The DBT budget wrapper gates roughly twenty portable nodes and
            // fails CLOSED on a pin it is not calibrated for. Nothing else
            // notices: a truncated node reads like a fast one. This asserts end
            // to end that the wrapper still REACHES its wrapped command at the
            // recorded pin.
            (root.join("ci/run-with-reverie-dbt-budget-test.sh"), vec![]),
            (
                root.join("ci/compat-envelope/scorecard.rs"),
                vec!["self-test-and-check"],
            ),
            (
                root.join("ci/compat-envelope/pressure-test.rs"),
                vec!["self-test"],
            ),
            // The removed shell front door accumulated plan/scheduler/receipt
            // guards that now belong to the Rust validate driver. Exercise
            // those brackets without executing the validation DAG.
            (root.join("scripts/validate.rs"), vec!["--self-test"]),
        ],
        audit_jobs,
    );
    audit_cli_brackets(root);
    let cells = audit_expected_plan(root, manifests);
    println!(
        "PASS: {} YAML manifests, {} required cells",
        manifests.documents.len(),
        cells
    );
    ExitCode::SUCCESS
}

fn audit_cli_brackets(root: &Path) {
    let executable = std::env::current_exe().unwrap_or_else(|error| fail(error));
    for option in [
        "--lane",
        "--category",
        "--test",
        "--mode",
        "--backend",
        "--results",
        "--junit",
        "--format",
        "--jobs",
    ] {
        let status = Command::new(&executable)
            .args(["plan", option])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| fail(error));
        if status.success() {
            fail(format!("missing value for {option} was accepted"));
        }
        let status = Command::new(&executable)
            .args(["plan", option, ""])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| fail(error));
        if status.success() {
            fail(format!("empty value for {option} was accepted"));
        }
    }
    let output = Command::new(&executable)
        .args([
            "plan",
            "--lane",
            "portable",
            "--ci-only",
            "--format",
            "json",
        ])
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| fail(error));
    let cells = serde_json::from_slice::<Vec<JsonValue>>(&output.stdout).unwrap_or_default();
    if !output.status.success() || cells.is_empty() {
        fail("complete CLI control was refused or selected no cells");
    }
    for argv in [
        vec!["run", "--jobs", "0"],
        vec!["run", "--jobs", "not-a-number"],
        vec!["run", "--jobs", "2", "--jobs", "3"],
        vec!["plan", "--jobs", "2"],
    ] {
        let status = Command::new(&executable)
            .args(argv)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| fail(error));
        if status.success() {
            fail("invalid --jobs control was accepted");
        }
    }
}

fn run_manifest_plan(root: &Path) {
    let manifest_plan = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .map(|parent| parent.join("hermit-manifest-plan"))
        })
        .unwrap_or_else(|| root.join("target/debug/hermit-manifest-plan"));
    let status = Command::new(&manifest_plan)
        .args(["--format", "json"])
        .current_dir(root)
        .stdout(Stdio::null())
        .status()
        .unwrap_or_else(|error| {
            fail(format!(
                "cannot execute {}: {error}",
                manifest_plan.display()
            ))
        });
    if !status.success() {
        fail(format!(
            "{} rejected the manifest or validation surface",
            manifest_plan.display()
        ));
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PlanCellIdentity {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

impl PlanCellIdentity {
    fn from_json(row: &JsonValue) -> Result<Self, String> {
        let field = |name: &str| {
            row.get(name)
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("plan row has no string `{name}`: {row}"))
        };
        Ok(Self {
            lane: field("lane")?,
            category: field("category")?,
            test: field("test")?,
            mode: field("mode")?,
            backend: field("backend")?,
        })
    }

    fn display(&self) -> String {
        format!(
            "{}/{}/{}/{}@{}",
            self.lane, self.category, self.test, self.mode, self.backend
        )
    }
}

fn unique_plan_rows(label: &str, rows: Vec<JsonValue>) -> Result<BTreeSet<String>, String> {
    let physical = rows.len();
    let mut identities = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    let mut normalized = BTreeSet::new();
    for row in rows {
        let identity = PlanCellIdentity::from_json(&row)?;
        if !identities.insert(identity.clone()) {
            duplicates.insert(identity);
        }
        normalized.insert(serde_json::to_string(&row).map_err(|error| error.to_string())?);
    }
    if !duplicates.is_empty() {
        let names = duplicates
            .iter()
            .map(PlanCellIdentity::display)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "{label} contains {physical} physical rows but only {} unique identities; duplicate identities: {names}",
            identities.len()
        ));
    }
    Ok(normalized)
}

fn required_plan_rows(manifests: &ManifestSet) -> (usize, Vec<JsonValue>) {
    let cells = manifests
        .select(&Selection {
            population: Some(Population::Required),
            ..Selection::default()
        })
        .unwrap_or_else(|e| fail(e));
    let mut actual = cells
        .iter()
        .map(|cell| {
            let capabilities = cell
                .test
                .requires
                .iter()
                .filter_map(|token| requires_capability(token).ok().flatten())
                .collect::<BTreeSet<_>>();
            let mut row = serde_json::json!({
                "test": cell.id.test,
                "category": cell.category,
                "lane": cell.test.lane,
                "mode": cell.id.mode,
                "backend": cell.id.backend,
            });
            if !capabilities.is_empty() {
                row["requires_host_capabilities"] = serde_json::json!(capabilities);
            }
            row
        })
        .collect::<Vec<_>>();
    actual.sort_by_key(|row| {
        PlanCellIdentity::from_json(row).expect("required plan rows have complete identities")
    });
    (cells.len(), actual)
}

fn expected_plan_document(root: &Path, manifests: &ManifestSet) -> JsonValue {
    let (_, cells) = required_plan_rows(manifests);
    let mut remaining = cells
        .into_iter()
        .map(|row| {
            let identity = PlanCellIdentity::from_json(&row)
                .expect("required plan rows have complete identities");
            (identity, row)
        })
        .collect::<BTreeMap<_, _>>();
    let mut cells = Vec::with_capacity(remaining.len());
    let path = root.join("ci/expected-e2e-plan.json");
    if let Ok(source) = fs::read(&path) {
        let current: JsonValue = serde_json::from_slice(&source)
            .unwrap_or_else(|error| fail(format!("cannot parse {}: {error}", path.display())));
        let current = current["cells"]
            .as_array()
            .unwrap_or_else(|| fail(format!("{} has no cells array", path.display())));
        let mut seen = BTreeSet::new();
        for row in current {
            let identity = PlanCellIdentity::from_json(row)
                .unwrap_or_else(|error| fail(format!("{}: {error}", path.display())));
            if !seen.insert(identity.clone()) {
                fail(format!(
                    "{} contains duplicate identity {}",
                    path.display(),
                    identity.display()
                ));
            }
            if let Some(row) = remaining.remove(&identity) {
                cells.push(row);
            }
        }
    }
    cells.extend(remaining.into_values());
    serde_json::json!({
        "schema": EXPECTED_PLAN_SCHEMA,
        "cells": cells,
    })
}

fn print_expected_plan(root: &Path, manifests: &ManifestSet) -> ExitCode {
    println!(
        "{}",
        serde_json::to_string_pretty(&expected_plan_document(root, manifests)).unwrap()
    );
    ExitCode::SUCCESS
}

fn audit_expected_plan(root: &Path, manifests: &ManifestSet) -> usize {
    let (cell_count, actual) = required_plan_rows(manifests);
    let expected: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap()).unwrap();
    if expected.get("schema").and_then(JsonValue::as_u64) != Some(EXPECTED_PLAN_SCHEMA) {
        fail(format!(
            "ci/expected-e2e-plan.json schema must be {EXPECTED_PLAN_SCHEMA}; regenerate it with `target/debug/test-harness expected-plan`"
        ));
    }
    let expected = expected["cells"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| fail("ci/expected-e2e-plan.json has no cells array"));
    let actual =
        unique_plan_rows("manifest required selection", actual).unwrap_or_else(|error| fail(error));
    let expected =
        unique_plan_rows("ci/expected-e2e-plan.json", expected).unwrap_or_else(|error| fail(error));
    if actual != expected {
        fail("required E2E plan changed; update ci/expected-e2e-plan.json in the same review");
    }
    cell_count
}

fn run_audit(root: &Path, program: &Path, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .unwrap_or_else(|error| fail(format!("cannot execute {}: {error}", program.display())));
    if !status.success() {
        // 127 IS AN ENVIRONMENT FAULT, NOT A FAILED AUDIT, AND SAYING "failed"
        // FOR BOTH COSTS A CI RUN ITS WHOLE E2E COVERAGE. These programs carry
        // `#!/usr/bin/env -S rust-script --force`; when that interpreter is
        // absent the kernel never runs the script and the shell reports 127.
        // The old message -- "tests/manifest-cli.rs self-test failed" -- reads
        // as the self-test having run and found a defect. Measured on hermit
        // run 32512027583: it had not run at all, and because build-debug gates
        // every e2e shard, a missing tool was read as a product break.
        if status.code() == Some(127) {
            fail(format!(
                "cannot run {}: exited 127, which means its interpreter was not found, \
                 not that the audit failed. This program runs under \
                 `#!/usr/bin/env -S rust-script --force`; install it with \
                 `cargo install rust-script` or put it on PATH (on a dev box it is \
                 usually ~/.cargo/bin, which a non-login shell does not inherit).",
                program.display()
            ));
        }
        fail(format!("{} {} failed", program.display(), args.join(" ")));
    }
}

fn run_audits_parallel(root: &Path, audits: &[(PathBuf, Vec<&str>)], jobs: usize) {
    let mut results = std::iter::repeat_with(|| None)
        .take(audits.len())
        .collect::<Vec<Option<Result<Output, String>>>>();
    for_each_parallel(
        audits.len(),
        ScheduledWorkerCapacity::new(jobs),
        |index, emit| {
            let (program, args) = &audits[index];
            let result = Command::new(program)
                .args(args)
                .current_dir(root)
                .output()
                .map_err(|error| format!("cannot execute {}: {error}", program.display()));
            let _ = emit(result, false);
        },
        |index, result, _| {
            results[index] = Some(result);
            true
        },
    );

    for ((program, args), result) in audits.iter().zip(results) {
        let output = result
            .expect("every validation audit worker returns one result")
            .unwrap_or_else(|error| fail(error));
        std::io::stdout()
            .write_all(&output.stdout)
            .unwrap_or_else(|error| {
                fail(format!(
                    "cannot replay {} stdout: {error}",
                    program.display()
                ))
            });
        std::io::stderr()
            .write_all(&output.stderr)
            .unwrap_or_else(|error| {
                fail(format!(
                    "cannot replay {} stderr: {error}",
                    program.display()
                ))
            });
        if !output.status.success() {
            if output.status.code() == Some(127) {
                fail(format!(
                    "cannot run {}: exited 127, which means its interpreter was not found, \
                     not that the audit failed. This program runs under \
                     `#!/usr/bin/env -S rust-script --force`; install it with \
                     `cargo install rust-script` or put it on PATH (on a dev box it is \
                     usually ~/.cargo/bin, which a non-login shell does not inherit).",
                    program.display()
                ));
            }
            fail(format!("{} {} failed", program.display(), args.join(" ")));
        }
    }
    println!(
        "test-harness: completed {} independent validation audits with up to {} concurrent workers",
        audits.len(),
        jobs.min(audits.len())
    );
}

fn audit_determinism_stress_evidence(root: &Path) {
    let program = root.join("tests/e2e/lib/determinism-stress/common.sh");
    let status = Command::new(&program)
        .env("DETERMINISM_STRESS_EVIDENCE_SELF_TEST", "1")
        .current_dir(root)
        .status()
        .unwrap_or_else(|error| fail(format!("cannot execute {}: {error}", program.display())));
    if !status.success() {
        fail(format!(
            "{} failed its comparison-evidence self-test",
            program.display()
        ));
    }
}

fn read_dag(path: &Path) -> Result<dagrun::DagConfig, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    dagrun::dag_from_json(&text)
        .map_err(|error| format!("{}: invalid DAG JSON: {error}", path.display()))
}

fn command_jobs(command: &str) -> Result<Option<i64>, String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let mut jobs = None;
    let mut index = 0;
    while index < words.len() {
        if words[index] == "--jobs" {
            let value = words
                .get(index + 1)
                .ok_or_else(|| "manifest command has --jobs without a value".to_string())?
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| "manifest command has invalid --jobs value".to_string())?;
            if jobs.replace(value).is_some() {
                return Err("manifest command repeats --jobs".into());
            }
            index += 1;
        }
        index += 1;
    }
    Ok(jobs)
}

const PREBUILT_COMMAND_PREFIX: &str = r#"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; "#;
const PINNED_COMMAND_PREFIX: &str = "./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo ";
const PINNED_COMMAND_SEPARATOR: &str = r#" -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c "$1"' bash "#;

fn shell_quote_one(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&byte))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn command_runs_exactly(command: &str, inner: &str) -> bool {
    let expected = format!("{PREBUILT_COMMAND_PREFIX}{inner}");
    if command == expected {
        return true;
    }
    let Some(rest) = command.strip_prefix(PINNED_COMMAND_PREFIX) else {
        return false;
    };
    let Some((forwarded, quoted_inner)) = rest.split_once(PINNED_COMMAND_SEPARATOR) else {
        return false;
    };
    let words = forwarded.split_whitespace().collect::<Vec<_>>();
    let (pairs, remainder) = words.as_chunks::<2>();
    if words.is_empty()
        || !remainder.is_empty()
        || pairs.iter().any(|pair| {
            pair[0] != "--env"
                || pair[1].is_empty()
                || !pair[1]
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        })
    {
        return false;
    }
    let unique = pairs.iter().map(|pair| pair[1]).collect::<BTreeSet<_>>();
    unique.len() == pairs.len() && quoted_inner == shell_quote_one(&expected)
}

fn audit_dag_correspondence(root: &Path, manifests: &ManifestSet) -> Result<(), String> {
    let committed_path = root.join("ci/dag/validate.json");
    let committed = read_dag(&committed_path)?;
    for lane in ["portable", "privileged"] {
        let path = &committed_path;
        let dag = dagrun::select_steps_by_labels(&committed, &[lane.to_string()])
            .map_err(|error| format!("{}: cannot select label {lane}: {error}", path.display()))?;
        if dag
            .steps
            .iter()
            .any(|step| step.cmd.contains("test_harness.sh"))
        {
            return Err(format!(
                "{} still invokes the removed shell harness",
                path.display()
            ));
        }
        let mut ids = BTreeSet::new();
        for step in &dag.steps {
            let id = format!("{}.{}", step.group, step.job);
            if !ids.insert(id.clone()) {
                return Err(format!("{} contains duplicate node {id}", path.display()));
            }
        }
        for step in &dag.steps {
            for dependency in &step.deps {
                if !ids.contains(dependency) {
                    return Err(format!(
                        "{} node {}.{} names missing dependency {dependency}",
                        path.display(),
                        step.group,
                        step.job
                    ));
                }
            }
        }
        if dag
            .steps
            .iter()
            .filter(|step| command_runs_exactly(&step.cmd, "target/debug/test-harness validate"))
            .count()
            != 1
        {
            return Err(format!(
                "{} must contain exactly one Rust metadata validation node",
                path.display()
            ));
        }
        let build =
            format!("target/debug/test-harness build --lane {lane} --ci-only --allow-empty");
        if dag
            .steps
            .iter()
            .filter(|step| command_runs_exactly(&step.cmd, &build))
            .count()
            != 2
        {
            return Err(format!(
                "{} must contain exactly the host and pinned-root Rust manifest build nodes",
                path.display()
            ));
        }
        let expected = manifests
            .documents
            .iter()
            .filter(|document| document.test.iter().any(|test| test.lane == lane))
            .map(|document| document.bucket.clone())
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        for step in dag.steps.iter().filter(|step| {
            step.manifest
                .as_ref()
                .is_some_and(|manifest| manifest.lane == lane)
        }) {
            let manifest = step.manifest.as_ref().ok_or_else(|| {
                format!("{}.{} lacks typed manifest identity", step.group, step.job)
            })?;
            let dagrun::DagManifest {
                lane: manifest_lane,
                category,
                ..
            } = manifest;
            if manifest_lane != lane {
                return Err(format!(
                    "{}.{} records lane {} in the {lane} DAG",
                    step.group, step.job, manifest_lane
                ));
            }
            let selector = format!(
                "target/debug/test-harness run --lane {lane} --category {} --ci-only --allow-empty --prebuilt",
                category
            );
            if !step.cmd.contains(&selector) {
                return Err(format!(
                    "{}.{} does not execute its typed selector literally",
                    step.group, step.job
                ));
            }
            if let Some(jobs) = command_jobs(&step.cmd)? {
                let demand = step
                    .hint
                    .resources
                    .get("manifest_guest")
                    .copied()
                    .unwrap_or(0);
                let cap = dag
                    .resource_caps
                    .get("manifest_guest")
                    .copied()
                    .unwrap_or(0);
                if demand != jobs
                    || cap < jobs
                    || step.hint.preferred_inner_jobs != Some(jobs)
                    || step.jobs_flag.as_deref() != Some("")
                {
                    return Err(format!(
                        "{}.{} runs --jobs {jobs} but declares manifest_guest={demand}, cap={cap}, preferred_inner_jobs={:?}, jobs_flag={:?}",
                        step.group, step.job, step.hint.preferred_inner_jobs, step.jobs_flag
                    ));
                }
            }
            if !actual.insert(category.clone()) {
                return Err(format!(
                    "{} has duplicate manifest bucket {}",
                    path.display(),
                    category
                ));
            }
        }
        if actual != expected {
            return Err(format!(
                "{} manifest buckets differ: expected={expected:?} actual={actual:?}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn audit_validation_levels_policy(workflow: &str) -> Result<(), String> {
    for variable in [
        "VALIDATE_GATE_TIMEOUT_SECONDS",
        "VALIDATE_GATE_CPU_TIMEOUT_SECONDS",
        "SUPER_REPETITIONS",
    ] {
        if workflow
            .lines()
            .any(|line| line.trim_start().starts_with(&format!("{variable}:")))
        {
            return Err(format!(
                "validation-levels.yml still sets {variable}, which would rewrite or conflict with the committed DAG"
            ));
        }
    }
    for command in [
        "ci/run-dag.sh privileged",
        "./scripts/validate.rs super --no-label-pr",
    ] {
        if !workflow.contains(command) {
            return Err(format!(
                "validation-levels.yml no longer invokes the committed-DAG path {command:?}"
            ));
        }
    }
    Ok(())
}

fn audit_budget_ordering(root: &Path) -> Result<(), String> {
    audit_workflow_run_dag_runners(root)?;
    let committed = read_dag(&root.join("ci/dag/validate.json"))?;
    let portable = dagrun::select_steps_by_labels(&committed, &["hosted-portable".into()])
        .map_err(|error| format!("cannot select portable DAG steps: {error}"))?;
    let privileged = dagrun::select_steps_by_labels(&committed, &["hosted-privileged".into()])
        .map_err(|error| format!("cannot select privileged DAG steps: {error}"))?;
    for (lane, dag) in [("portable", &portable), ("privileged", &privileged)] {
        for step in &dag.steps {
            if step.timeout <= 0 {
                return Err(format!(
                    "{lane} node {}.{} has no derivable wall budget",
                    step.group, step.job
                ));
            }
            if lane == "portable" {
                let Some(value) = step.cmd.strip_prefix("CARGO_BUILD_JOBS=") else {
                    continue;
                };
                let jobs = value
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or_else(|| {
                        format!(
                            "{lane} node {}.{} has an invalid CARGO_BUILD_JOBS prefix",
                            step.group, step.job
                        )
                    })?;
                if step.hint.preferred_inner_jobs != Some(jobs) {
                    return Err(format!(
                        "{lane} node {}.{} declares CARGO_BUILD_JOBS={jobs} without matching preferred_inner_jobs",
                        step.group, step.job
                    ));
                }
            }
        }
    }

    let portable_workflow = parse_yaml(&root.join(".github/workflows/ci-portable.yml"))?;
    let debug_bound = workflow_job_timeout(&portable_workflow, "test-debug")? * 60;
    let release_bound = workflow_job_timeout(&portable_workflow, "test-release")? * 60;
    let shards: JsonValue = serde_json::from_slice(
        &fs::read(root.join("ci/portable-shards.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("invalid portable shard map: {e}"))?;
    let portable_steps = portable
        .steps
        .iter()
        .map(|step| (format!("{}.{}", step.group, step.job), step))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut current = BTreeSet::new();
    for (key, job, bound) in [
        ("debug_shards", "test-debug", debug_bound),
        ("release_shards", "test-release", release_bound),
    ] {
        for node in shards[key]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|shard| shard["nodes"].as_array().into_iter().flatten())
        {
            let node = node
                .as_str()
                .ok_or_else(|| format!("{key} contains a non-string node"))?;
            let step = portable_steps
                .get(node)
                .ok_or_else(|| format!("portable shard names missing DAG node {node}"))?;
            let timeout = u64::try_from(step.timeout).map_err(|_| {
                format!("portable node {node} has invalid timeout {}", step.timeout)
            })?;
            if timeout >= bound {
                current.insert(format!(
                    "{node} {timeout}s >= {bound}s (job {job} timeout-minutes)"
                ));
            }
        }
    }

    let validation_levels =
        fs::read_to_string(root.join(".github/workflows/validation-levels.yml"))
            .map_err(|error| error.to_string())?;
    audit_validation_levels_policy(&validation_levels)?;

    let privileged_workflow = fs::read_to_string(root.join(".github/workflows/ci-privileged.yml"))
        .map_err(|e| e.to_string())?;
    audit_privileged_unboxed_guard(&privileged_workflow)?;
    if privileged_workflow
        .matches("continue-on-error: true")
        .count()
        != 1
    {
        return Err(
            "privileged workflow must contain exactly one diagnostic continue-on-error".into(),
        );
    }
    let launcher_line = privileged_workflow
        .lines()
        .find(|line| line.contains("ci/run-dag.sh privileged"))
        .ok_or_else(|| "cannot find privileged launcher command".to_string())?;
    let launcher_bound = command_timeout_seconds(launcher_line)?
        .ok_or_else(|| "cannot derive privileged launcher timeout".to_string())?;
    let privileged_yaml = parse_yaml(&root.join(".github/workflows/ci-privileged.yml"))?;
    let privileged_job_bound = workflow_job_timeout(&privileged_yaml, "privileged")? * 60;
    let declared_step_budgets = workflow_step_timeout_sum(&privileged_yaml, "privileged")?;
    if privileged_job_bound <= declared_step_budgets {
        return Err(format!(
            "privileged job {privileged_job_bound}s must exceed {declared_step_budgets}s of explicit inner step budgets"
        ));
    }
    let critical_path = dag_critical_path(&privileged)?;
    if launcher_bound <= critical_path + 30 {
        return Err(format!(
            "privileged launcher {launcher_bound}s must exceed {critical_path}s DAG critical path plus 30s runner overhead"
        ));
    }
    for step in &privileged.steps {
        let timeout = u64::try_from(step.timeout).map_err(|_| {
            format!(
                "privileged node {}.{} has invalid timeout {}",
                step.group, step.job, step.timeout
            )
        })?;
        if timeout >= launcher_bound {
            current.insert(format!(
                "{}.{} {timeout}s >= {launcher_bound}s (privileged launcher wrapper)",
                step.group, step.job
            ));
        }
    }

    let expected = fs::read_to_string(root.join("ci/budget-inversions-baseline.txt"))
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if current != expected {
        let new = current.difference(&expected).cloned().collect::<Vec<_>>();
        let fixed = expected.difference(&current).cloned().collect::<Vec<_>>();
        return Err(format!(
            "budget-inversion baseline drifted: new={new:?} fixed-but-listed={fixed:?}"
        ));
    }
    println!(
        "budget ordering: {} baseline inversion(s), {} portable sharded + {} privileged nodes checked",
        current.len(),
        shards["debug_shards"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(shards["release_shards"].as_array().into_iter().flatten())
            .map(|shard| shard["nodes"].as_array().map_or(0, Vec::len))
            .sum::<usize>(),
        privileged.steps.len()
    );
    Ok(())
}

fn audit_workflow_run_dag_runners(root: &Path) -> Result<(), String> {
    for relative in [
        ".github/workflows/ci-dag.yml",
        ".github/workflows/ci-privileged.yml",
        ".github/workflows/validation-levels.yml",
    ] {
        let workflow = parse_yaml(&root.join(relative))?;
        audit_run_dag_workflow_runner(relative, &workflow)?;
    }
    Ok(())
}

fn audit_run_dag_workflow_runner(label: &str, workflow: &YamlValue) -> Result<(), String> {
    const PORTABLE: &str = "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable ${{ inputs.max_mem != '' && format('--max-mem {0}', inputs.max_mem) || '' }} -v";
    const DAG_PRIVILEGED: &str =
        "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 -v";
    const VALIDATION_PRIVILEGED: &str = "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v";
    const STANDALONE_PRIVILEGED: &str = "if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n  echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n  exit 2\nfi\ntimeout --foreground --kill-after=10s 1560s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --unsafe-no-cgroups --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v";
    const FIXTURES: &[&str] = &[
        "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v",
        "timeout --foreground --kill-after=10s 1560s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -v",
    ];
    let expected: &[(&str, &str)] = match label {
        ".github/workflows/ci-dag.yml" => &[
            ("dag-portable", PORTABLE),
            ("dag-privileged", DAG_PRIVILEGED),
        ],
        ".github/workflows/ci-privileged.yml" => &[("privileged", STANDALONE_PRIVILEGED)],
        ".github/workflows/validation-levels.yml" => &[("full", VALIDATION_PRIVILEGED)],
        "fixture" => &[],
        _ => return Err(format!("workflow {label} has no expected run-dag commands")),
    };
    let jobs = workflow["jobs"]
        .as_mapping()
        .ok_or_else(|| format!("workflow {label} has no jobs mapping"))?;
    let mut consumers = Vec::new();
    for (job_name, job) in jobs {
        let job_name = job_name.as_str().unwrap_or("<non-string job>");
        let steps = job["steps"]
            .as_sequence()
            .ok_or_else(|| format!("workflow {label} job {job_name} has no steps"))?;
        for (step_index, step) in steps.iter().enumerate() {
            let Some(run) = step.get("run").and_then(YamlValue::as_str) else {
                continue;
            };
            let normalized = run
                .chars()
                .filter(|character| !matches!(character, '\\' | '\'' | '"'))
                .collect::<String>();
            if !normalized.contains("ci/run-dag.sh") {
                continue;
            }
            consumers.push((job_name, run.trim_end()));
            if label == "fixture" && !FIXTURES.contains(&run.trim_end()) {
                return Err(format!(
                    "workflow {label} job {job_name} run-dag step {step_index} is not an exact allowed Rust-runner command"
                ));
            }
        }
    }
    if label == "fixture" && consumers.len() != 1 {
        return Err(format!("workflow {label} has no ci/run-dag.sh consumer"));
    }
    if label != "fixture" && consumers != expected {
        return Err(format!(
            "workflow {label} run-dag commands differ from the exact Rust-runner commands: actual={consumers:?} expected={expected:?}"
        ));
    }
    Ok(())
}

fn audit_privileged_unboxed_guard(workflow: &str) -> Result<(), String> {
    const ACTIONS_GUARD: &str = "        if [[ ${GITHUB_ACTIONS:-} != true ]]; then";
    const REFUSAL: &str = "          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2";
    const FAIL_CLOSED: &str = "          exit 2";

    for (line, description) in [
        (ACTIONS_GUARD, "exact GitHub Actions context guard"),
        (REFUSAL, "explicit outside-Actions refusal"),
        (FAIL_CLOSED, "nonzero outside-Actions exit"),
    ] {
        if workflow
            .lines()
            .filter(|candidate| *candidate == line)
            .count()
            != 1
        {
            return Err(format!(
                "privileged workflow must contain exactly one {description}"
            ));
        }
    }

    if workflow.matches("--unsafe-no-cgroups").count() != 1 {
        return Err(
            "privileged workflow must select explicit unboxed execution exactly once".into(),
        );
    }
    if workflow
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| line.contains("--allow-cgroup-failure"))
    {
        return Err(
            "privileged workflow must not execute with broad --allow-cgroup-failure".into(),
        );
    }
    Ok(())
}

fn dag_critical_path(dag: &dagrun::DagConfig) -> Result<u64, String> {
    let steps = dag
        .steps
        .iter()
        .map(|step| (format!("{}.{}", step.group, step.job), step))
        .collect::<std::collections::BTreeMap<_, _>>();
    fn visit(
        id: &str,
        steps: &std::collections::BTreeMap<String, &dagrun::Step>,
        active: &mut BTreeSet<String>,
        memo: &mut std::collections::BTreeMap<String, u64>,
    ) -> Result<u64, String> {
        if let Some(value) = memo.get(id) {
            return Ok(*value);
        }
        if !active.insert(id.to_string()) {
            return Err(format!("DAG dependency cycle reaches {id}"));
        }
        let step = steps
            .get(id)
            .ok_or_else(|| format!("DAG critical path references missing node {id}"))?;
        let predecessor = step
            .deps
            .iter()
            .map(|dependency| visit(dependency, steps, active, memo))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        active.remove(id);
        let timeout = u64::try_from(step.timeout)
            .map_err(|_| format!("DAG node {id} has invalid timeout {}", step.timeout))?;
        let value = predecessor
            .checked_add(timeout)
            .ok_or_else(|| format!("DAG critical path overflows at {id}"))?;
        memo.insert(id.to_string(), value);
        Ok(value)
    }
    let mut memo = std::collections::BTreeMap::new();
    let mut maximum = 0;
    for id in steps.keys() {
        maximum = maximum.max(visit(id, &steps, &mut BTreeSet::new(), &mut memo)?);
    }
    Ok(maximum)
}

fn parse_yaml(path: &Path) -> Result<YamlValue, String> {
    serde_yaml::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: invalid YAML: {e}", path.display()))
}

fn workflow_job_timeout(workflow: &YamlValue, job: &str) -> Result<u64, String> {
    workflow["jobs"][job]["timeout-minutes"]
        .as_u64()
        .ok_or_else(|| format!("workflow job {job} has no numeric timeout-minutes"))
}

fn command_timeout_seconds(command: &str) -> Result<Option<u64>, String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let Some(index) = words.iter().position(|word| *word == "timeout") else {
        return Ok(None);
    };
    let budget = words[index + 1..]
        .iter()
        .find(|word| !word.starts_with('-'))
        .and_then(|word| word.trim_end_matches('\\').strip_suffix('s'))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| format!("cannot derive timeout budget from `{command}`"))?;
    Ok(Some(budget))
}

fn workflow_step_timeout_sum(workflow: &YamlValue, job: &str) -> Result<u64, String> {
    let steps = workflow["jobs"][job]["steps"]
        .as_sequence()
        .ok_or_else(|| format!("workflow job {job} has no steps"))?;
    let mut sum = 0;
    for run in steps
        .iter()
        .filter_map(|step| step.get("run"))
        .filter_map(YamlValue::as_str)
    {
        for line in run.lines() {
            let words = line.split_whitespace().collect::<Vec<_>>();
            for index in 0..words.len() {
                if words[index] == "timeout" {
                    let budget = words[index + 1..]
                        .iter()
                        .find(|word| !word.starts_with('-'))
                        .and_then(|word| word.trim_end_matches('\\').strip_suffix('s'))
                        .and_then(|value| value.parse::<u64>().ok())
                        .ok_or_else(|| format!("cannot derive timeout budget from `{line}`"))?;
                    sum += budget;
                }
            }
        }
    }
    Ok(sum)
}

fn print_plan(manifests: &ManifestSet, args: &Args, population: Population) -> ExitCode {
    let mut selection = args.selection.clone();
    selection.population = Some(population);

    // ⚠️ AN UNKNOWN TEST ID IS A REFUSAL HERE, AND THIS IS THE ONLY SUBCOMMAND THAT
    // NEEDED IT. `run` and `build` already fail closed on an empty selection
    // (`filters selected no cells`), but `plan` printed an empty list and exited 0 --
    // measured 2026-08-26: `plan --lane portable --test no-such-test-xyz` is rc=0, and
    // so is the same command with a REAL id, so its exit code carried no information in
    // either direction. Anything driving a bisection off `plan` therefore reads a typo
    // as "nothing failed here" and converges, confidently, on the wrong commit.
    //
    // ⚠️ AND THE CHECK IS "UNKNOWN ID", NOT "EMPTY RESULT", WHICH IS NOT THE SAME FIX.
    // `print_plan` also serves `audit-gaps` (Population::Disabled), where an empty
    // answer legitimately means NO GAPS. Mirroring run's `cells.is_empty()` guard here
    // would turn that good answer into a failure. Asking whether the named id exists at
    // all separates the two: a real id with no cells in this population still prints
    // nothing and exits 0.
    if let Some(id) = selection.test.as_deref() {
        if !manifests.knows_test(id) {
            fail(format!(
                "unknown test id {id:?}: it is not in any manifest. An empty plan for a \
                 real id means that population has no cells; an empty plan for an id \
                 that does not exist means the filter is wrong, and refusing is what \
                 stops a bisection reading a typo as a pass."
            ));
        }
    }

    let cells = manifests.select(&selection).unwrap_or_else(|e| fail(e));
    if args.format == "json" {
        println!("{}", serde_json::to_string(&cells.iter().map(|c| {
            let backend = if population == Population::Disabled && c.id.mode == "naked" {
                Some("native")
            } else {
                c.id.backend.as_deref()
            };
            serde_json::json!({"test":c.id.test,"category":c.category,"lane":c.test.lane,"mode":c.id.mode,"backend":backend})
        }).collect::<Vec<_>>()).unwrap());
    } else {
        for cell in cells {
            let backend = if population == Population::Disabled && cell.id.mode == "naked" {
                "native"
            } else {
                cell.id.backend.as_deref().unwrap_or("-")
            };
            println!(
                "{}\t{}\t{}\t{}\t{}",
                cell.test.lane, cell.category, cell.id.test, cell.id.mode, backend
            );
        }
    }
    ExitCode::SUCCESS
}

fn build(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let mut selection = args.selection.clone();
    if selection.population.is_none() {
        selection.population = Some(if selection.include_manual {
            Population::Enabled
        } else {
            Population::Required
        });
    }
    let cells = manifests.select(&selection).unwrap_or_else(|e| fail(e));
    if cells.is_empty() && !args.allow_empty {
        fail("filters selected no cells");
    }
    let capacity = build_worker_capacity(args);
    let context = RunContext::from_env(root.to_path_buf(), false).unwrap_or_else(|e| fail(e));
    let mut seen = BTreeSet::new();
    let cells = cells
        .into_iter()
        .filter(|cell| seen.insert(cell.id.test.clone()))
        .collect::<Vec<_>>();
    let mut results = std::iter::repeat_with(|| None)
        .take(cells.len())
        .collect::<Vec<Option<Result<(), String>>>>();
    for_each_parallel(
        cells.len(),
        capacity,
        |index, emit| {
            let cell = &cells[index];
            let dir = context.build_root.join(cell.id.test.replace('/', "-"));
            let result = hermit_manifest_plan::runner::prepare_test(&context, cell, &dir).map(drop);
            let _ = emit(result, false);
        },
        |index, result, _| {
            results[index] = Some(result);
            true
        },
    );
    let mut failed = false;
    for (cell, result) in cells.iter().zip(results) {
        match result.expect("every fixture preparation worker returns one result") {
            Ok(_) => println!("BUILT {}", cell.id.test),
            Err(e) => {
                eprintln!("ERROR {}: {e}", cell.id.test);
                failed = true;
            }
        }
    }
    println!(
        "test-harness: completed {} preparation(s) with up to {} concurrent worker(s)",
        cells.len(),
        capacity.workers_for(cells.len())
    );
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn audit_compile(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let context = RunContext::from_env(root.to_path_buf(), false).unwrap_or_else(|e| fail(e));
    let mut checked = 0;
    let mut failed = false;
    for (category, inherited_timeout_seconds, inherited_cpu_timeout_seconds, test) in
        manifests.all_tests()
    {
        if args
            .selection
            .lane
            .as_deref()
            .is_some_and(|lane| lane != test.lane)
            || args
                .selection
                .category
                .as_deref()
                .is_some_and(|value| value != category)
            || args
                .selection
                .test
                .as_deref()
                .is_some_and(|value| value != test.id)
            || !test
                .program
                .as_deref()
                .is_some_and(|program| program.ends_with(".c"))
        {
            continue;
        }
        let verify = test
            .modes
            .get("verify")
            .expect("validated manifests carry verify");
        let backend = verify
            .backends_enabled
            .first()
            .cloned()
            .unwrap_or_else(|| "ptrace".into());
        let timeout_seconds = verify
            .timeout_seconds
            .get(&backend)
            .copied()
            .unwrap_or(inherited_timeout_seconds);
        let cpu_timeout_seconds = verify
            .cpu_timeout_seconds
            .get(&backend)
            .copied()
            .unwrap_or(inherited_cpu_timeout_seconds);
        let cell = hermit_manifest_plan::runner::SelectedCell {
            category: category.into(),
            test: test.clone(),
            id: hermit_manifest_plan::runner::CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some(backend),
            },
            enabled: false,
            timeout_seconds,
            cpu_timeout_seconds,
        };
        checked += 1;
        let dir = context
            .result_root
            .join("audit-compile")
            .join(test.id.replace('/', "-"));
        if let Err(e) = hermit_manifest_plan::runner::prepare_test(&context, &cell, &dir) {
            eprintln!("ERROR {}: {e}", test.id);
            failed = true;
        }
    }
    if checked == 0 {
        fail("compile audit compiled zero guests");
    }
    if failed {
        ExitCode::FAILURE
    } else {
        println!("compile audit: {checked} compiled");
        ExitCode::SUCCESS
    }
}

/// Execute `count` independent items with at most `jobs` workers, delivering
/// each emitted value to `consume` immediately and waiting for its
/// acknowledgement before the worker may continue.
///
/// The consumer stays on the calling thread so durable publication is
/// serialized even while the expensive cell executions overlap. This is
/// deliberately not a collect-then-publish helper: an outer bucket timeout
/// must not discard rows that completed before the timeout, and a retry must
/// not start before the prior attempt is flushed.
fn for_each_parallel<T: Send>(
    count: usize,
    capacity: ScheduledWorkerCapacity,
    execute: impl Fn(usize, &mut dyn FnMut(T, bool) -> bool) + Sync,
    mut consume: impl FnMut(usize, T, bool) -> bool,
) {
    if count == 0 {
        return;
    }
    let workers = capacity.workers_for(count);
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel::<(usize, T, bool, mpsc::SyncSender<bool>)>();
    thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let execute = &execute;
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let mut emit = |value, will_retry| {
                        let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
                        if sender.send((index, value, will_retry, ack_sender)).is_err() {
                            return false;
                        }
                        ack_receiver.recv().unwrap_or(false)
                    };
                    execute(index, &mut emit);
                }
            });
        }
        drop(sender);
        for (index, value, will_retry, ack_sender) in receiver {
            let acknowledged = consume(index, value, will_retry);
            let _ = ack_sender.send(acknowledged);
        }
    });
}

fn run_with_retry<T>(
    first_attempt: u64,
    mut execute: impl FnMut(u64) -> T,
    mut retryable: impl FnMut(&T) -> bool,
    mut emit: impl FnMut(T, bool) -> bool,
) {
    assert!(
        (1..=MAX_ATTEMPTS_PER_CELL).contains(&first_attempt),
        "first cell attempt must be within the shared attempt cap"
    );
    for attempt in first_attempt..=MAX_ATTEMPTS_PER_CELL {
        let result = execute(attempt);
        let will_retry = retryable(&result) && attempt < MAX_ATTEMPTS_PER_CELL;
        if !emit(result, will_retry) || !will_retry {
            break;
        }
    }
}

/// Retry only a completed product observation.
///
/// The failure class is the producer-owned distinction between a measured
/// product failure and a run that could not produce a product verdict. Do not
/// infer retryability from the human-readable reason or from the broad
/// `FAIL`/`ERROR` presentation outcome: doing so doubled every `no_result` row
/// in one failed validation without producing any additional information.
fn cell_result_is_retryable(outcome: &str, failure_class: Option<FailureClass>) -> bool {
    match failure_class {
        Some(FailureClass::ProductFailure) => outcome == "FAIL",
        Some(FailureClass::UnderstoodInfrastructureFailure) => false,
        Some(FailureClass::UnderstoodPrerequisiteFailure) => false,
        Some(FailureClass::NoResult) | None => false,
    }
}

fn run(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let mut selection = args.selection.clone();
    if selection.population.is_none() {
        selection.population = Some(if selection.include_manual {
            Population::Enabled
        } else {
            Population::Required
        });
    }
    let cells = manifests.select(&selection).unwrap_or_else(|e| fail(e));
    if cells.is_empty() && !args.allow_empty {
        fail("filters selected no cells");
    }
    let capacity = scheduled_worker_capacity(args);
    let context = RunContext::from_env(root.to_path_buf(), args.prebuilt)
        .unwrap_or_else(|e| fail(e))
        .with_scheduled_worker_capacity(capacity);
    for (capability, verdict) in &context.host_capabilities {
        eprintln!(
            "Host capability {}: {} — {}",
            capability.value(),
            if verdict.present { "PRESENT" } else { "ABSENT" },
            verdict.evidence
        );
    }
    let results_path = args.results.clone().unwrap_or_else(|| {
        context
            .result_root
            .join(&context.run_id)
            .join("results.jsonl")
    });
    let junit = args
        .junit
        .clone()
        .unwrap_or_else(|| context.result_root.join(&context.run_id).join("junit.xml"));
    prepare_result_path(&results_path).unwrap_or_else(|error| {
        fail(format!(
            "cannot prepare result path {}: {error}",
            results_path.display()
        ))
    });
    let mut indexed_results = Vec::new();
    let mut attempt_results = vec![Vec::new(); cells.len()];
    let mut failed = false;
    // Sum the producer-owned CPU measurement from EVERY executed observation,
    // including a failed row that is retried. This is specifically cell CPU;
    // the harness process itself remains in the enclosing DAG cgroup.
    let mut cell_cpu_usage_usec = Some(0u64);
    let mut cpu_measurements = 0usize;
    let expected = cells.len();
    for_each_parallel(
        expected,
        capacity,
        |index, emit| {
            let cell = &cells[index];
            if let Some((_, reason)) =
                host_inapplicable_reason(&cell.test.requires, &context.host_capabilities)
            {
                let _ = emit(host_inapplicable_result(&context, cell, reason), false);
                return;
            }

            run_with_retry(
                context.attempt,
                |attempt| {
                    let attempt_context = context.with_attempt(attempt);
                    match run_cell(&attempt_context, cell) {
                        Ok(result) => result,
                        Err(error) => infrastructure_error_result(&attempt_context, cell, error),
                    }
                },
                |result| cell_result_is_retryable(result.outcome.as_str(), result.failure_class),
                emit,
            );
        },
        |index, mut result: CellResult, will_retry| {
            accumulate_cell_cpu_usage(
                &mut cell_cpu_usage_usec,
                &mut cpu_measurements,
                &result.outcome,
                result.cpu_usage_usec,
            );
            // Publish before announcing the outcome. After a visible PASS line,
            // the complete typed row is already present even if the containing
            // bucket is killed before its JUnit/summary epilogue. The worker
            // waits for this acknowledgement before starting a retry.
            let published = if let Err(error) = append_result(&results_path, &result) {
                eprintln!(
                    "ERROR {} ({}/{}): completed cell result could not be published: {error}",
                    result.test,
                    result.mode,
                    result.backend.as_deref().unwrap_or("native")
                );
                result.outcome = "ERROR".into();
                result.result = None;
                result.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
                result.error_kind = Some("result-publication".into());
                result.reason = Some(format!(
                    "completed cell result could not be published: {error}"
                ));
                false
            } else {
                true
            };

            if result.outcome == "ERROR" {
                eprintln!(
                    "ERROR {} ({}/{}): {}",
                    result.test,
                    result.mode,
                    result.backend.as_deref().unwrap_or("native"),
                    result.reason.as_deref().unwrap_or("infrastructure error")
                );
            }
            // A FAILURE MUST SAY ENOUGH TO BE CLASSIFIED, NOT JUST COUNTED.
            let located = if result.outcome == "PASS" {
                String::new()
            } else if result.outcome == "HOST-INAPPLICABLE" {
                format!(
                    " {}",
                    result.reason.as_deref().unwrap_or("host-inapplicable")
                )
            } else {
                let coords = [
                    ("turn", result.first_divergent_scheduler_turn),
                    ("vns", result.first_divergent_virtual_nanoseconds),
                    ("rec", result.first_divergent_record),
                    ("sys", result.first_divergent_syscall),
                ]
                .iter()
                .filter_map(|(key, value)| value.map(|value| format!("{key}={value}")))
                .collect::<Vec<_>>();
                let mut suffix = String::new();
                if !coords.is_empty() {
                    suffix.push_str(&format!(" [{}]", coords.join(" ")));
                }
                if let Some(reason) = result.reason.as_deref() {
                    suffix.push_str(&format!(" {reason}"));
                }
                suffix.push_str(&format!("\n    evidence: {}", result.artifact_dir));
                suffix
            };
            let effective_will_retry = published && will_retry;
            let retry_note = if effective_will_retry {
                format!(
                    " [attempt {} of at most {}; retrying this cell only]",
                    result.attempt, MAX_ATTEMPTS_PER_CELL
                )
            } else {
                String::new()
            };
            println!(
                "{} {} ({}/{}){}{}",
                result.outcome,
                result.test,
                result.mode,
                result.backend.as_deref().unwrap_or("native"),
                retry_note,
                located
            );

            attempt_results[index].push(result);
            if !effective_will_retry {
                let result = match cell_result_after_retries(&attempt_results[index]) {
                    Ok(result) => result.clone(),
                    Err(error) => {
                        let mut result = attempt_results[index]
                            .last()
                            .expect("the current attempt was retained before reporting")
                            .clone();
                        result.outcome = "ERROR".into();
                        result.error_kind = Some("result-history".into());
                        result.reason = Some(error);
                        result
                    }
                };
                failed |= matches!(result.outcome.as_str(), "FAIL" | "ERROR");
                indexed_results.push((index, result));
            }
            published
        },
    );
    if indexed_results.len() != expected {
        eprintln!(
            "test-harness: only {} of {expected} selected cells returned a result",
            indexed_results.len()
        );
        failed = true;
    }
    indexed_results.sort_by_key(|(index, _)| *index);
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();
    if expected > 0 {
        println!(
            "test-harness: completed {} cell(s) with up to {} concurrent worker(s)",
            results.len(),
            capacity.workers_for(expected)
        );
    }
    let host_inapplicable = results
        .iter()
        .filter(|result| result.outcome == "HOST-INAPPLICABLE")
        .count();
    let cell_cpu_usage_usec = (cpu_measurements > 0)
        .then_some(cell_cpu_usage_usec)
        .flatten();
    if let Some(path) = std::env::var_os("DAGRUN_TEST_COUNTS_PATH") {
        let path = PathBuf::from(path);
        if let Err(error) =
            structured_test_results(&attempt_results).and_then(|report| report.write_current(&path))
        {
            eprintln!("test-harness: {error}");
            failed = true;
        }
    }
    if expected > 0 && host_inapplicable == expected {
        eprintln!(
            "test-harness: every one of the {expected} selected cell(s) was host-inapplicable; \
             a run that executed no cell is not a pass"
        );
        failed = true;
    }
    write_junit(&junit, &results).unwrap();
    let summary = serde_json::json!({
        "schema": 1,
        "cells": results.len(),
        "passed": results.iter().filter(|result| result.outcome == "PASS").count(),
        "failed": results.iter().filter(|result| result.outcome == "FAIL").count(),
        "errors": results.iter().filter(|result| result.outcome == "ERROR").count(),
        "host_inapplicable": host_inapplicable,
        "cell_cpu_usage_usec": cell_cpu_usage_usec,
        "host_inapplicable_cells": results
            .iter()
            .filter(|result| result.outcome == "HOST-INAPPLICABLE")
            .map(|result| serde_json::json!({
                "test": result.test,
                "mode": result.mode,
                "backend": result.backend,
                "reason": result.reason,
            }))
            .collect::<Vec<_>>(),
    });
    fs::write(
        results_path.parent().unwrap().join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use hermit_manifest_plan::runner::FailureClass;
    use hermit_manifest_plan::runner::ManifestSet;
    use hermit_manifest_plan::runner::ScheduledWorkerCapacity;

    use super::DEFAULT_BUILD_JOBS;
    use super::EXPECTED_PLAN_SCHEMA;
    use super::HostCapability;
    use super::HostCapabilityVerdict;
    use super::PINNED_COMMAND_PREFIX;
    use super::PINNED_COMMAND_SEPARATOR;
    use super::PREBUILT_COMMAND_PREFIX;
    use super::accumulate_cell_cpu_usage;
    use super::audit_privileged_unboxed_guard;
    use super::audit_run_dag_workflow_runner;
    use super::audit_validation_levels_policy;
    use super::build_worker_capacity;
    use super::cell_result_is_retryable;
    use super::command_jobs;
    use super::command_runs_exactly;
    use super::command_timeout_seconds;
    use super::expected_plan_document;
    use super::for_each_parallel;
    use super::host_inapplicable_reason;
    use super::parse;
    use super::run_with_retry;
    use super::scheduled_worker_capacity;
    use super::shell_quote_one;
    use super::structured_test_results_from_rows;
    use super::unique_plan_rows;

    #[test]
    fn generated_expected_plan_is_versioned_and_matches_the_tracked_file() {
        let root = super::root();
        let manifests = ManifestSet::load(&root).unwrap();
        let generated = expected_plan_document(&root, &manifests);
        assert_eq!(generated["schema"], EXPECTED_PLAN_SCHEMA);
        let tracked: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap())
                .unwrap();
        assert_eq!(tracked, generated);
    }

    fn duplicate_plan_fixture(mode: &str) -> Vec<serde_json::Value> {
        let mut rows = (0..307)
            .map(|index| {
                serde_json::json!({
                    "lane": "portable",
                    "category": "fixture",
                    "test": format!("fixture/test-{index:03}"),
                    "mode": if index == 0 { mode } else { "verify" },
                    "backend": "ptrace",
                })
            })
            .collect::<Vec<_>>();
        rows.push(rows[0].clone());
        rows
    }

    #[test]
    fn expected_plan_refuses_duplicate_comparable_and_custom_rows_before_set_comparison() {
        for mode in ["verify", "custom"] {
            let rows = duplicate_plan_fixture(mode);
            let error = unique_plan_rows("fixture expected plan", rows)
                .expect_err("308 physical rows with 307 identities must be refused");
            assert!(
                error.contains("308 physical rows but only 307 unique identities"),
                "{error}"
            );
            assert!(error.contains(&format!("fixture/test-000/{mode}@ptrace")));
        }
    }

    #[test]
    fn cell_cpu_summary_includes_retries_and_refuses_incomplete_measurements() {
        let mut total = Some(0);
        let mut measurements = 0;
        accumulate_cell_cpu_usage(&mut total, &mut measurements, "FAIL", Some(3));
        accumulate_cell_cpu_usage(&mut total, &mut measurements, "PASS", Some(4));
        accumulate_cell_cpu_usage(
            &mut total,
            &mut measurements,
            "HOST-INAPPLICABLE",
            Some(100),
        );
        assert_eq!(measurements, 2);
        assert_eq!(total, Some(7));

        accumulate_cell_cpu_usage(&mut total, &mut measurements, "ERROR", None);
        assert_eq!(measurements, 3);
        assert_eq!(total, None);
    }

    #[test]
    fn structured_test_results_are_machine_readable_and_exact_on_failure() {
        let path = std::env::temp_dir().join(format!(
            "hermit-manifest-counts-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        structured_test_results_from_rows([
            ("suite$passes".into(), true, 1),
            ("suite$fails".into(), false, 2),
        ])
        .unwrap()
        .write_current(&path)
        .unwrap();
        let counts: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            counts,
            serde_json::json!({
                "schema": 2,
                "executed_tests": 2,
                "filtered_tests": 0,
                "results": [
                    {"id": "suite$passes", "result": "pass", "attempts": 1},
                    {"id": "suite$fails", "result": "fail", "attempts": 2},
                ],
            })
        );
    }

    #[test]
    fn only_a_declared_absent_capability_withholds_a_cell() {
        let absent = BTreeMap::from([(
            HostCapability::CpuidFaulting,
            HostCapabilityVerdict {
                present: false,
                evidence: "planted absence".into(),
            },
        )]);
        let requires = vec!["linux".to_string(), "cpuid".to_string()];
        let (capabilities, reason) = host_inapplicable_reason(&requires, &absent).unwrap();
        assert_eq!(capabilities, ["cpuid-faulting"]);
        assert!(reason.contains("NOT RUN, NOT a pass, no coverage"));
        assert!(reason.contains("planted absence"));

        let undeclared = vec!["linux".to_string(), "ptrace".to_string()];
        assert!(host_inapplicable_reason(&undeclared, &absent).is_none());

        let present = BTreeMap::from([(
            HostCapability::CpuidFaulting,
            HostCapabilityVerdict {
                present: true,
                evidence: "planted presence".into(),
            },
        )]);
        assert!(host_inapplicable_reason(&requires, &present).is_none());
    }

    const GUARDED_WORKFLOW: &str = r#"    # --allow-cgroup-failure is documented here but not executed.
        if [[ ${GITHUB_ACTIONS:-} != true ]]; then
          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2
          exit 2
        fi
        timeout 720s ci/run-dag.sh privileged --unsafe-no-cgroups
"#;

    #[test]
    fn validation_levels_cannot_rewrite_committed_graph_policy() {
        let workflow = include_str!("../../../../.github/workflows/validation-levels.yml");
        assert!(audit_validation_levels_policy(workflow).is_ok());
        for planted in [
            "VALIDATE_GATE_TIMEOUT_SECONDS: 3600",
            "VALIDATE_GATE_CPU_TIMEOUT_SECONDS: 3600",
            "SUPER_REPETITIONS: 7",
        ] {
            let changed = format!("{workflow}\nenv:\n  {planted}\n");
            let error = audit_validation_levels_policy(&changed).unwrap_err();
            assert!(
                error.contains(planted.split(':').next().unwrap()),
                "{error}"
            );
        }
    }

    #[test]
    fn privileged_unboxed_execution_requires_the_exact_actions_guard() {
        assert!(audit_privileged_unboxed_guard(GUARDED_WORKFLOW).is_ok());
    }

    #[test]
    fn privileged_unboxed_execution_refuses_incomplete_guards() {
        for required in [
            "        if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n",
            "          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n",
            "          exit 2\n",
        ] {
            let incomplete = GUARDED_WORKFLOW.replacen(required, "", 1);
            assert!(audit_privileged_unboxed_guard(&incomplete).is_err());
        }
    }

    #[test]
    fn privileged_unboxed_execution_requires_one_explicit_opt_out() {
        let missing = GUARDED_WORKFLOW.replace(" --unsafe-no-cgroups", "");
        assert!(audit_privileged_unboxed_guard(&missing).is_err());

        let duplicate = format!("{GUARDED_WORKFLOW}# --unsafe-no-cgroups\n");
        assert!(audit_privileged_unboxed_guard(&duplicate).is_err());
    }

    #[test]
    fn privileged_unboxed_execution_rejects_broad_boxing_failure_acceptance() {
        let executable = format!("{GUARDED_WORKFLOW}        run: tool --allow-cgroup-failure\n");
        assert!(audit_privileged_unboxed_guard(&executable).is_err());
    }

    #[test]
    fn run_dag_workflows_use_a_structured_result_capable_runner() {
        for command in [
            "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v",
            "timeout --foreground --kill-after=10s 1560s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -v",
        ] {
            let workflow: serde_yaml::Value = serde_yaml::from_str(&format!(
                "jobs:\n  validation:\n    steps:\n      - run: {command}\n"
            ))
            .unwrap();
            assert!(audit_run_dag_workflow_runner("fixture", &workflow).is_ok());
        }
    }

    #[test]
    fn run_dag_workflows_refuse_python_overrides_even_when_multiline() {
        for assignment in [
            "DAGRUN_BIN=agent-utils/py/bin/dagrun",
            "DAGRUN_BIN=\"agent-utils/py/bin/dagrun\"",
            "DAGRUN_ENGINE=python",
            "DAGRUN_ENGINE='python'",
            "DAGRUN_ENGINE=py",
            "DAGRUN_ENGINE='py'",
        ] {
            let workflow: serde_yaml::Value = serde_yaml::from_str(&format!(
                "jobs:\n  validation:\n    steps:\n      - run: |\n          env \\\n            {assignment} \\\n            ci/run-dag.sh privileged -v\n"
            ))
            .unwrap();
            let error = audit_run_dag_workflow_runner("fixture", &workflow)
                .expect_err("a Python runner cannot consume structured-result DAGs");
            assert!(
                error.contains("not an exact allowed Rust-runner command"),
                "{error}"
            );
        }
    }

    #[test]
    fn run_dag_workflows_refuse_python_overrides_from_each_environment_scope() {
        for workflow in [
            "env:\n  DAGRUN_ENGINE: py\njobs:\n  validation:\n    steps:\n      - run: ci/run-dag.sh portable -v\n",
            "jobs:\n  validation:\n    env:\n      DAGRUN_ENGINE: python\n    steps:\n      - run: ci/run-dag.sh portable -v\n",
            "jobs:\n  validation:\n    steps:\n      - env:\n          DAGRUN_BIN: agent-utils/py/bin/dagrun\n        run: ci/run-dag.sh portable -v\n",
        ] {
            let workflow: serde_yaml::Value = serde_yaml::from_str(workflow).unwrap();
            let error = audit_run_dag_workflow_runner("fixture", &workflow)
                .expect_err("a Python runner cannot consume structured-result DAGs");
            assert!(
                error.contains("not an exact allowed Rust-runner command"),
                "{error}"
            );
        }
    }

    #[test]
    fn run_dag_workflows_follow_binary_precedence_and_refuse_dynamic_commands() {
        for command in [
            "DAGRUN_BIN=agent-utils/common/bin/dagrun ci/run-dag.sh portable -v",
            "export DAGRUN_ENGINE=py; ci/run-dag.sh portable -v",
            "env 'DAGRUN_ENGINE=py' ci/run-dag.sh portable -v",
            "env 'DAGRUN_BIN=agent-utils/py/bin/dagrun' ci/run-dag.sh portable -v",
            "echo 'env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v'",
            "if false; then env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v; fi",
            "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v; DAGRUN_ENGINE=py ci/run-dag\\.sh portable -v",
            "DAGRUN_ENGINE=py ci/run-dag.sh portable -v; ci/run-dag.sh privileged -v",
        ] {
            let workflow: serde_yaml::Value = serde_yaml::from_str(&format!(
                "jobs:\n  validation:\n    steps:\n      - run: {command}\n"
            ))
            .unwrap();
            assert!(audit_run_dag_workflow_runner("fixture", &workflow).is_err());
        }

        let expression: serde_yaml::Value = serde_yaml::from_str(
            "env:\n  DAGRUN_ENGINE: ${{ vars.DAGRUN_ENGINE }}\njobs:\n  validation:\n    steps:\n      - run: ci/run-dag.sh portable -v\n",
        )
        .unwrap();
        assert!(audit_run_dag_workflow_runner("fixture", &expression).is_err());

        let inherited_from_github_env: serde_yaml::Value = serde_yaml::from_str(
            "jobs:\n  validation:\n    steps:\n      - run: echo DAGRUN_ENGINE=py >> \"$GITHUB_ENV\"\n      - run: ci/run-dag.sh portable -v\n",
        )
        .unwrap();
        assert!(audit_run_dag_workflow_runner("fixture", &inherited_from_github_env).is_err());

        let inherited_values_are_cleared: serde_yaml::Value = serde_yaml::from_str(
            "env:\n  DAGRUN_ENGINE: python\njobs:\n  validation:\n    steps:\n      - run: echo DAGRUN_BIN=agent-utils/py/bin/dagrun >> \"$GITHUB_ENV\"\n      - env:\n          DAGRUN_BIN: agent-utils/py/bin/dagrun\n        run: env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v\n",
        )
        .unwrap();
        assert!(audit_run_dag_workflow_runner("fixture", &inherited_values_are_cleared).is_ok());
    }

    #[test]
    fn privileged_launcher_timeout_does_not_depend_on_an_env_prefix() {
        for command in [
            "timeout --foreground --kill-after=10s 1560s ci/run-dag.sh privileged -v",
            "timeout --foreground --kill-after=10s 1560s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -v",
        ] {
            assert_eq!(command_timeout_seconds(command).unwrap(), Some(1560));
        }
    }

    #[test]
    fn scheduled_jobs_uses_the_parsed_worker_capacity() {
        let default = scheduled_worker_capacity(&parse(std::iter::empty()));
        assert_eq!(default.configured(), 1);

        let explicit =
            scheduled_worker_capacity(&parse(["--jobs", "7"].into_iter().map(str::to_string)));
        assert_eq!(explicit.configured(), 7);
        assert_eq!(explicit.workers_for(12), 7);
        assert_eq!(explicit.workers_for(1), 1);
    }

    #[test]
    fn build_jobs_default_to_the_measured_useful_width_and_accept_an_override() {
        let default = build_worker_capacity(&parse(std::iter::empty()));
        assert_eq!(default.configured(), DEFAULT_BUILD_JOBS);

        let explicit =
            build_worker_capacity(&parse(["--jobs", "3"].into_iter().map(str::to_string)));
        assert_eq!(explicit.configured(), 3);
        assert_eq!(explicit.workers_for(2), 2);
    }

    #[test]
    fn parallel_runner_delivers_every_completion_before_returning() {
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let consumed = Mutex::new(Vec::new());
        for_each_parallel(
            8,
            ScheduledWorkerCapacity::new(4),
            |index, emit| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                assert!(emit(index, false));
            },
            |index, value, _| {
                consumed.lock().unwrap().push((index, value));
                true
            },
        );
        let mut rows = consumed.into_inner().unwrap();
        rows.sort_unstable();
        assert_eq!(rows, (0..8).map(|index| (index, index)).collect::<Vec<_>>());
        assert!(maximum.load(Ordering::SeqCst) > 1);
    }

    #[test]
    fn retry_waits_for_publication_and_stops_after_pass() {
        let published = AtomicUsize::new(0);
        let executions = AtomicUsize::new(0);
        let rows = Mutex::new(Vec::new());
        for_each_parallel(
            1,
            ScheduledWorkerCapacity::new(1),
            |_, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        if attempt == 2 {
                            assert_eq!(published.load(Ordering::SeqCst), 1);
                        }
                        executions.fetch_add(1, Ordering::SeqCst);
                        attempt
                    },
                    |attempt| *attempt == 1,
                    emit,
                );
            },
            |_, attempt, will_retry| {
                rows.lock().unwrap().push((attempt, will_retry));
                published.fetch_add(1, Ordering::SeqCst);
                true
            },
        );
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(rows.into_inner().unwrap(), [(1, true), (2, false)]);
    }

    #[test]
    fn retry_policy_is_exhaustive_over_typed_failure_classes() {
        use FailureClass::NoResult;
        use FailureClass::ProductFailure;
        use FailureClass::UnderstoodInfrastructureFailure;
        use FailureClass::UnderstoodPrerequisiteFailure;

        for (outcome, failure_class, expected) in [
            ("FAIL", Some(ProductFailure), true),
            ("ERROR", Some(ProductFailure), false),
            ("FAIL", Some(NoResult), false),
            ("ERROR", Some(NoResult), false),
            ("ERROR", Some(UnderstoodPrerequisiteFailure), false),
            ("ERROR", Some(UnderstoodInfrastructureFailure), false),
            (
                "HOST-INAPPLICABLE",
                Some(UnderstoodPrerequisiteFailure),
                false,
            ),
            ("PASS", None, false),
            ("FAIL", None, false),
        ] {
            assert_eq!(
                cell_result_is_retryable(outcome, failure_class),
                expected,
                "outcome={outcome} failure_class={failure_class:?}"
            );
        }
    }

    #[test]
    fn no_result_batch_and_passing_peer_each_execute_once() {
        const NO_RESULT_CELLS: usize = 178;
        const CELL_COUNT: usize = NO_RESULT_CELLS + 1;

        let executions = (0..CELL_COUNT)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>();
        let rows = Mutex::new(Vec::new());
        for_each_parallel(
            CELL_COUNT,
            ScheduledWorkerCapacity::new(8),
            |index, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        executions[index].fetch_add(1, Ordering::SeqCst);
                        if index < NO_RESULT_CELLS {
                            (attempt, "ERROR", Some(FailureClass::NoResult))
                        } else {
                            (attempt, "PASS", None)
                        }
                    },
                    |(_, outcome, failure_class)| cell_result_is_retryable(outcome, *failure_class),
                    emit,
                );
            },
            |index, (attempt, _, _), will_retry| {
                rows.lock().unwrap().push((index, attempt, will_retry));
                true
            },
        );

        assert!(
            executions
                .iter()
                .all(|count| count.load(Ordering::SeqCst) == 1)
        );
        let mut rows = rows.into_inner().unwrap();
        rows.sort_unstable();
        assert_eq!(rows.len(), CELL_COUNT);
        assert!(
            rows.iter()
                .all(|(_, attempt, will_retry)| *attempt == 1 && !will_retry)
        );
    }

    #[test]
    fn product_failure_keeps_one_retry() {
        let executions = AtomicUsize::new(0);
        let mut rows = Vec::new();
        run_with_retry(
            1,
            |attempt| {
                executions.fetch_add(1, Ordering::SeqCst);
                if attempt == 1 {
                    (attempt, "FAIL", Some(FailureClass::ProductFailure))
                } else {
                    (attempt, "PASS", None)
                }
            },
            |(_, outcome, failure_class)| cell_result_is_retryable(outcome, *failure_class),
            |(attempt, _, _), will_retry| {
                rows.push((attempt, will_retry));
                true
            },
        );

        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(rows, [(1, true), (2, false)]);
    }

    #[test]
    fn production_run_retries_only_product_failures() {
        use std::path::Path;
        use std::path::PathBuf;
        use std::process::Command;
        use std::process::ExitCode;

        use hermit_manifest_plan::runner::CellResult;
        use hermit_manifest_plan::runner::ObservedResult;
        use serde_json::json;

        const CHILD_FIXTURE: &str = "HERMIT_HARNESS_RETRY_TEST_FIXTURE";
        const TEST_NAME: &str = "tests::production_run_retries_only_product_failures";
        if let Some(fixture) = std::env::var_os(CHILD_FIXTURE) {
            let fixture = PathBuf::from(fixture);
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let manifests = ManifestSet::load(&fixture).unwrap();
            let args = parse(
                [
                    "--mode".into(),
                    "naked".into(),
                    "--jobs".into(),
                    "2".into(),
                    "--results".into(),
                    fixture.join("results.jsonl").to_string_lossy().into_owned(),
                    "--junit".into(),
                    fixture.join("junit.xml").to_string_lossy().into_owned(),
                ]
                .into_iter(),
            );
            super::validate_args("run", &args);
            // Exercise the real run() callback, publication, result reduction and
            // epilogue. Its product failure and non-product errors must stay red.
            assert_eq!(super::run(&root, &manifests, &args), ExitCode::FAILURE);
            return;
        }

        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-native-retry-{}",
            std::process::id()
        ));
        fs::create_dir(&fixture).unwrap();
        let manifests = fixture.join("tests/e2e/manifests");
        fs::create_dir_all(&manifests).unwrap();
        fs::write(
            manifests.join("defaults.yaml"),
            "schema: 3\ntimeout_seconds: 2\ncpu_timeout_seconds: 1\n",
        )
        .unwrap();
        let disabled = json!({
            "ci": false,
            "backends_enabled": [],
            "backends_disabled": {
                "ptrace": "This control executes native commands only",
                "dbt": "This control executes native commands only",
                "kvm": "This control executes native commands only",
                "sabre": "This control executes native commands only",
                "liteinst": "This control executes native commands only"
            }
        });
        let modes = json!({
            "naked": {
                "ci": false,
                "ci_disabled_reason": "Native retry control is explicitly selected",
                "backends_enabled": ["native"],
                "runs": 1,
                "assert": {"min_distinct": 1}
            },
            "verify": disabled,
            "chaos": disabled,
            "replay": disabled,
            "custom": disabled
        });
        let missing = fixture.join("missing-native-program");
        let recipes = [
            ("infra", vec![missing.to_string_lossy().into_owned()]),
            ("pass", vec!["/bin/true".into()]),
            (
                "product",
                vec!["/bin/sh".into(), "-c".into(), "exit 23".into()],
            ),
            (
                "recovers",
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "case \"$E2E_TMPDIR\" in *-attempt-2/tmp) exit 0;; *) exit 23;; esac".into(),
                ],
            ),
            ("timeout", vec!["/bin/sleep".into(), "10".into()]),
        ]
        .into_iter()
        .map(|(id, direct)| {
            json!({
                "id": format!("retry/{id}"),
                "description": "Native production-callback retry control",
                "lane": "portable",
                "occasional": false,
                "direct": direct,
                "observation": {"status": true, "stdout": true, "stderr": true},
                "modes": modes
            })
        })
        .collect::<Vec<_>>();
        fs::write(
            manifests.join("retry.yaml"),
            serde_json::to_vec(&json!({"schema": 3, "bucket": "retry", "test": recipes})).unwrap(),
        )
        .unwrap();
        // Only the isolated child receives execution environment changes. A
        // missing Hermit path makes the optional metadata/help probes inert;
        // all five cells use the actual native execution path.
        let output = Command::new("timeout")
            .args(["--kill-after=2s", "25s"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(CHILD_FIXTURE, &fixture)
            .env("HERMIT_BIN", fixture.join("missing-hermit"))
            .env("E2E_RESULT_ROOT", fixture.join("artifacts"))
            .env("E2E_BUILD_ROOT", fixture.join("build"))
            .env("E2E_RUN_ID", "native-retry-control")
            .env("E2E_MACHINE_SHORTNAME", "native-retry-control")
            .env("E2E_KERNEL_VERSION", "native-retry-control")
            .env("DAGRUN_TEST_COUNTS_PATH", fixture.join("counts.json"))
            .output()
            .unwrap();
        fs::write(fixture.join("child.stdout"), &output.stdout).unwrap();
        fs::write(fixture.join("child.stderr"), &output.stderr).unwrap();
        assert!(
            output.status.success(),
            "native run control failed: {}\n{}\n{}",
            fixture.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let rows = fs::read_to_string(fixture.join("results.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
            .collect::<Vec<_>>();
        let mut histories = BTreeMap::<String, Vec<&CellResult>>::new();
        for row in &rows {
            row.require_current_classification().unwrap();
            row.require_current_timeout_policy().unwrap();
            assert_eq!(row.mode, "naked");
            assert_eq!(row.backend, None);
            assert_eq!(row.execution_cpu_timeout_seconds, Some(1));
            assert_eq!(row.execution_wall_timeout_seconds, Some(2));
            assert_eq!(row.timeout_seconds, 2);
            histories.entry(row.test.clone()).or_default().push(row);
        }
        assert_eq!(
            histories.len(),
            5,
            "every selected identity must remain present"
        );
        for (id, expected) in [
            (
                "infra",
                vec![(
                    1,
                    "ERROR",
                    Some(FailureClass::UnderstoodInfrastructureFailure),
                )],
            ),
            ("pass", vec![(1, "PASS", None)]),
            (
                "product",
                vec![
                    (1, "FAIL", Some(FailureClass::ProductFailure)),
                    (2, "FAIL", Some(FailureClass::ProductFailure)),
                ],
            ),
            (
                "recovers",
                vec![
                    (1, "FAIL", Some(FailureClass::ProductFailure)),
                    (2, "PASS", None),
                ],
            ),
            ("timeout", vec![(1, "FAIL", Some(FailureClass::NoResult))]),
        ] {
            let history = &histories[&format!("retry/{id}")];
            assert_eq!(
                history
                    .iter()
                    .map(|r| (r.attempt, r.outcome.as_str(), r.failure_class))
                    .collect::<Vec<_>>(),
                expected,
                "actual production retry history for {id}; artifacts: {}",
                fixture.display()
            );
        }
        assert_eq!(
            histories["retry/timeout"][0].result,
            Some(ObservedResult::Timeout)
        );
        assert!(histories["retry/timeout"][0].attempts[0].timed_out);
        assert!(histories["retry/infra"][0].attempts.is_empty());
        let counts: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.join("counts.json")).unwrap()).unwrap();
        assert_eq!(
            counts,
            json!({
                "schema": 2,
                "executed_tests": 5,
                "filtered_tests": 0,
                "results": [
                    {"id": "retry/infra [native/naked]", "result": "fail", "attempts": 1},
                    {"id": "retry/pass [native/naked]", "result": "pass", "attempts": 1},
                    {"id": "retry/product [native/naked]", "result": "fail", "attempts": 2},
                    {"id": "retry/recovers [native/naked]", "result": "pass", "attempts": 2},
                    {"id": "retry/timeout [native/naked]", "result": "fail", "attempts": 1}
                ]
            })
        );
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.join("summary.json")).unwrap()).unwrap();
        for (name, expected) in [
            ("cells", 5),
            ("passed", 2),
            ("failed", 2),
            ("errors", 1),
            ("host_inapplicable", 0),
        ] {
            assert_eq!(summary[name], expected, "summary {name}");
        }
        assert!(
            summary["cell_cpu_usage_usec"].is_null(),
            "missing CPU evidence must stay unknown"
        );
        let junit = fs::read_to_string(fixture.join("junit.xml")).unwrap();
        assert!(junit.contains("tests=\"5\" failures=\"2\" errors=\"1\" skipped=\"0\""));
        assert_eq!(junit.matches("<testcase ").count(), 5);
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn retry_stops_after_two_failures() {
        let executions = AtomicUsize::new(0);
        let mut rows = Vec::new();
        run_with_retry(
            1,
            |attempt| {
                executions.fetch_add(1, Ordering::SeqCst);
                attempt
            },
            |_| true,
            |attempt, will_retry| {
                rows.push((attempt, will_retry));
                true
            },
        );
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(rows, [(1, true), (2, false)]);
    }

    #[test]
    fn retry_starting_at_second_attempt_cannot_create_a_third() {
        let mut rows = Vec::new();
        run_with_retry(
            2,
            |attempt| attempt,
            |_| true,
            |attempt, will_retry| {
                rows.push((attempt, will_retry));
                true
            },
        );
        assert_eq!(rows, [(2, false)]);
    }

    #[test]
    fn one_failing_cell_does_not_rerun_its_passing_peer() {
        let executions = [AtomicUsize::new(0), AtomicUsize::new(0)];
        let terminal = Mutex::new(Vec::new());
        for_each_parallel(
            2,
            ScheduledWorkerCapacity::new(2),
            |index, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        executions[index].fetch_add(1, Ordering::SeqCst);
                        (index, attempt)
                    },
                    |(index, attempt)| *index == 0 && *attempt == 1,
                    emit,
                );
            },
            |index, _, will_retry| {
                if !will_retry {
                    terminal.lock().unwrap().push(index);
                }
                true
            },
        );
        assert_eq!(executions[0].load(Ordering::SeqCst), 2);
        assert_eq!(executions[1].load(Ordering::SeqCst), 1);
        let mut terminal = terminal.into_inner().unwrap();
        terminal.sort_unstable();
        assert_eq!(terminal, [0, 1]);
    }

    #[test]
    fn publication_refusal_prevents_the_retry() {
        let executions = AtomicUsize::new(0);
        for_each_parallel(
            1,
            ScheduledWorkerCapacity::new(1),
            |_, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        executions.fetch_add(1, Ordering::SeqCst);
                        attempt
                    },
                    |_| true,
                    emit,
                );
            },
            |_, _, _| false,
        );
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manifest_command_audit_accepts_only_exact_host_or_pinned_commands() {
        let inner = "target/debug/test-harness validate";
        let host = format!("{PREBUILT_COMMAND_PREFIX}{inner}");
        assert!(command_runs_exactly(&host, inner));
        let pinned = format!(
            "{PINNED_COMMAND_PREFIX}--env E2E_RESULT_ROOT --env VALIDATE_VERBOSITY{PINNED_COMMAND_SEPARATOR}{}",
            shell_quote_one(&host)
        );
        assert!(command_runs_exactly(&pinned, inner));
        assert!(!command_runs_exactly(
            &format!("{PREBUILT_COMMAND_PREFIX}true # {inner}"),
            inner
        ));
        assert!(!command_runs_exactly(&format!("{pinned} && true"), inner));
        assert!(!command_runs_exactly(
            &format!(
                "{PINNED_COMMAND_PREFIX}--env E2E_RESULT_ROOT --env E2E_RESULT_ROOT{PINNED_COMMAND_SEPARATOR}{}",
                shell_quote_one(&host)
            ),
            inner
        ));
    }

    #[test]
    fn manifest_jobs_parser_rejects_missing_invalid_and_duplicate_widths() {
        assert_eq!(
            command_jobs("test-harness run --jobs 20").unwrap(),
            Some(20)
        );
        assert_eq!(command_jobs("test-harness run").unwrap(), None);
        for command in [
            "test-harness run --jobs",
            "test-harness run --jobs 0",
            "test-harness run --jobs no",
            "test-harness run --jobs 2 --jobs 3",
        ] {
            assert!(command_jobs(command).is_err(), "accepted {command}");
        }
    }
}
