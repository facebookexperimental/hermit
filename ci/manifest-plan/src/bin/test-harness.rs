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

use dagrun::TestAttemptOutcome;
use dagrun::TestAttemptResult;
use dagrun::TestResult;
use dagrun::TestResults;
use hermit_manifest_plan::cli_help::is_help_flag;
use hermit_manifest_plan::imported_results;
use hermit_manifest_plan::imported_results::IMPORT_RESULTS_ENV;
use hermit_manifest_plan::parity;
use hermit_manifest_plan::parity::ParityCellId;
use hermit_manifest_plan::runner::CellId;
use hermit_manifest_plan::runner::CellResult;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::MAX_ATTEMPTS_PER_CELL;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::RELAXED_LEVEL_ARGS;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::STRICTNESS_LEVEL_ENV;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::SelectedCell;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::append_result;
use hermit_manifest_plan::runner::backend_capability;
use hermit_manifest_plan::runner::cell_declared_stdout;
use hermit_manifest_plan::runner::cell_expected_guest_exit;
use hermit_manifest_plan::runner::cell_relaxations;
use hermit_manifest_plan::runner::cell_result_after_retries;
use hermit_manifest_plan::runner::cell_result_and_attempts_after_retries;
use hermit_manifest_plan::runner::checked_add_cpu_usage;
use hermit_manifest_plan::runner::diagnostic_failure_reason;
use hermit_manifest_plan::runner::host_inapplicable_result;
use hermit_manifest_plan::runner::host_input_change_only;
use hermit_manifest_plan::runner::host_strictness;
use hermit_manifest_plan::runner::is_diagnostic_cell;
use hermit_manifest_plan::runner::prepare_result_path;
use hermit_manifest_plan::runner::refused_on_an_inexact_branch_counter;
use hermit_manifest_plan::runner::requires_capability;
use hermit_manifest_plan::runner::retries_product_failures;
use hermit_manifest_plan::runner::run_cell;
use hermit_manifest_plan::runner::skid_overshoot_only_reports;
use hermit_manifest_plan::runner::validate_source_snapshot;
use hermit_manifest_plan::runner::write_junit;
use hermit_manifest_plan::self_test_selection;
use hermit_manifest_plan::stress_series::HostCapabilities;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapability;
use hermit_manifest_plan::stress_series::HostCapabilityVerdict;
use hermit_manifest_plan::validation_dag::PINNED_ROOT_COMMAND_GUARD;
use hermit_manifest_plan::validation_dag::TOOL_SELF_TESTS;
use hermit_manifest_plan::validation_dag::ToolSelfTest;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

const EXPECTED_PLAN_SCHEMA: u64 = 1;
const EXPECTED_PLAN_PATH: &str = "ci/expected-e2e-plan.json";
const OPTIONAL_CELLS_PATH: &str = "ci/optional-e2e-cells.txt";
const DEFAULT_BUILD_JOBS: usize = 16;
const DEFAULT_VALIDATE_AUDIT_JOBS: usize = 2;
const PREBUILT_RUST_SCRIPTS_REQUIRED: &str = "HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED";
const VALIDATE_AUDIT_JOBS_ENV: &str = "HERMIT_VALIDATE_AUDIT_JOBS";

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
  parity <compare|export>          Measure parity from retained logs, or export ledger rows
  sync-cells <--check|--write>     Regenerate the expected plan and optional cells
  selftest <NAME>                  Run one repository tool's self-test

Selection options:
  --lane <portable|privileged>
  --category <CATEGORY>
  --test <ID>
  --mode <verify|chaos|replay|naked|custom>
  --backend <ptrace|dbt|kvm|sabre|liteinst>
  --exclude-backend <ptrace|dbt|kvm|sabre|liteinst>
                                   Omit that backend's cells; may repeat
  --label <LABEL[,LABEL...]>       Keep tests carrying any named label; may repeat
  --exclude-category <CATEGORY>    Omit that manifest category's cells; may repeat
  --ci-only                        Select required CI cells
  --include-occasional             Include occasional cells
  --include-manual                 Include manual cells; requires exact test and mode
  --probe-disabled                 Run one exact disabled cell

Source options:
  --repo-root <DIR>                Read manifests and programs from DIR and run hermit
                                   there (default: the checkout this binary was built in)
  --source-sha <SHA>               DIR is a clean `git archive` of commit SHA whose files
                                   Git does not track: take the test inventory from its
                                   files and record SHA as hermit_sha instead of asking git

Execution and output options:
  --prebuilt                       Reuse prepared test programs (run only)
  --diagnostic-results             Write dagrun structured-result schema 4, which
                                   reports a diagnostic cell's failure without
                                   failing the node (run only)
  --allow-empty                    Permit an empty explicit CI selection
  --no-retry                       Run each selected cell exactly once: a product
                                   failure is final instead of earning the one
                                   framework retry. For flake measurement, where
                                   a retry would hide the failure rate (run only)
  --results <PATH>                 Write JSONL cell results to PATH
  --junit <PATH>                   Write JUnit output to PATH
  --tpx-json <PATH>                Write one Tpx HPHP-JSON test_done line per final cell
                                   to PATH (run only)
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
  HERMIT_BIN=<PATH>                      Hermit executable; a relative path is under the
                                         repository root (default: target/debug/hermit)
  HERMIT_E2E_EMPTY_WORKDIR=/test         Use the isolated /test working directory
  E2E_KEEP_VERIFY_LOGS=1                 Retain verify logs (one golden log after a match)
  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=<N> Positive finite CPU-time multiplier
  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=<N> Positive finite wall-time multiplier";

/// Read by `run` only: the parity post-pass after the determinism cells.
const RUN_ENVIRONMENT: &str =
    "  E2E_PARITY_SELECT=<TEST@BACKEND,...>   Also measure these parity cells after the run
  E2E_PARITY_POST_PASS=0                 Skip the parity post-pass (default: 1)
  E2E_IMPORT_RESULTS=<ROOT>              Run no cell: publish the rows another run left in
                                         ROOT/<lane>/manifest_<category>/; a selected cell
                                         with no row is an ERROR. The parity post-pass
                                         reads the verify logs restored in
                                         ROOT/retained-verify-logs/";

const SYNC_CELLS_HELP: &str = "\
Usage: test-harness sync-cells <--check|--write> [--repo-root <DIR>]

Regenerate the manifest-derived files that the scorecard is generated from,
and the inventory of optional cells:

  ci/expected-e2e-plan.json        the required plan, rows in their committed order
  ci/optional-e2e-cells.txt        enabled cells that are not required (ci: false)

The parity population needs no file: it is derived from the manifests (every
test whose ptrace verify cell full validation selects, on every candidate
backend), and generate-parity-cells snapshots it.

A cell flip makes one existing manifest cell required: its backend joins the
mode's `backends_enabled` and `ci`, or its `ci: false` becomes true. The diff of
ci/expected-e2e-plan.json is the record of every change to the required cells.
`test-harness validate` refuses a tree where any of these files differs from
this output.

ci/sync-cell-config.sh runs this and then every generator downstream of it;
run that rather than this alone.

Options:
  --check                          Exit 1 naming each stale file; write nothing
  --write                          Rewrite the stale files
  --repo-root <DIR>                Read and write DIR (default: the checkout this
                                   binary was built in)
  -h, --help                       Print this help

Exit status: 0 when the files match (--check) or were written (--write), 1 when
--check finds a stale file, 2 for an unreadable input or a usage error.";

const PARITY_HELP: &str = "\
Usage: test-harness parity compare --artifacts <DIR> --cell <TEST@BACKEND> [OPTIONS]
       test-harness parity export --e2e-root <DIR> [--expected-scope <FILE>]

Measure parity cells from the logs a finished `run` retained, as its own
post-pass does, without running any guest. Each cell compares TEST's verify
run on BACKEND with its verify run on ptrace, using `hermit log-diff`, and
prints one parity record per cell. Every cell named is measured, whichever
process ran its verify cells.

It writes only below DIR/parity-compare: parity.jsonl, parity.status.json
(which names the hermit binary that compared the logs and its SHA-256), and
the goldens and log-diff reports under DIR/parity-compare/parity/. The run's
results.jsonl, JUnit, summary.json, parity.jsonl, parity.status.json and
parity/ are only read. A golden the run wrote is reused once the reference's
retained log is gone, except in an imported run, whose summary.json names its
import ROOT: each log is read only from the directory the ingest restored it
into below ROOT/retained-verify-logs/, and no golden is reused.

Options:
  --artifacts <DIR>                The directory holding the run's results.jsonl
  --cell <TEST@BACKEND>            A parity cell to measure; repeatable
  --output <PATH>                  Write records to PATH (default: DIR/parity-compare/parity.jsonl)
  --jobs <N>                       Run at most N comparisons concurrently (default: 1)
  -h, --help                       Print this help

Environment:
  HERMIT_BIN=<PATH>                      Hermit executable; a relative path is under the
                                         repository root (default: target/debug/hermit)

`parity export` reads every DIR/<lane>/<node>/parity.status.json and the
parity.jsonl beside it, and prints one parity ledger source row per parity
cell those post-passes owed, as JSONL on stdout, for series.py append-parity.
A complete post-pass yields one row per record. A failed or running one
yields the records it wrote plus a record-missing row for each other cell its
status names. With --expected-scope, a JSON object mapping \"<lane>/<node>\" to
the \"<test>@<backend>\" cells that node should have measured, a planned node
with no status yields a record-missing row per expected cell, and so does
each expected cell a node's status leaves out of its scope. It runs no guest
and writes nothing. Any inconsistency (a record that fails validation, a
count that disagrees with its status, a record outside the scope) is an error
and prints no rows. validate, which must not lose a cell, instead appends a
record-missing row for each cell the refused node's expected scope owed.

Export options:
  --e2e-root <DIR>                 The run's e2e result root, holding <lane>/<node>/
  --expected-scope <FILE>          The cells each planned node owed (see above)";

const FILTER_OPTIONS: &str = "  --lane <portable|privileged>
  --category <CATEGORY>
  --test <ID>
  --mode <verify|chaos|replay|naked|custom>
  --backend <ptrace|dbt|kvm|sabre|liteinst>
  --exclude-backend <ptrace|dbt|kvm|sabre|liteinst>
                                   Omit that backend's cells; may repeat
  --label <LABEL[,LABEL...]>       Keep tests carrying any named label; may repeat
  --exclude-category <CATEGORY>    Omit that manifest category's cells; may repeat";

const REPO_ROOT_OPTION: &str =
    "  --repo-root <DIR>                Read manifests and programs from DIR (default: the
                                   checkout this binary was built in)";

const SOURCE_SHA_OPTION: &str =
    "  --source-sha <SHA>               DIR is a clean `git archive` of commit SHA whose files
                                   Git does not track: take the test inventory from its
                                   files and record SHA as hermit_sha instead of asking git";

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
             --tpx-json <PATH>                Write one Tpx HPHP-JSON test_done line per final cell\n  \
             --jobs <N>                       Run at most N cells concurrently",
            CommandEnvironment::Run,
        ),
        "parity" => {
            println!("{PARITY_HELP}");
            return true;
        }
        "sync-cells" => {
            println!("{SYNC_CELLS_HELP}");
            return true;
        }
        "selftest" => {
            println!("{}", selftest_help());
            return true;
        }
        _ => return false,
    };
    // Every command accepts --repo-root, so every usage line takes options.
    println!("Usage: test-harness {command} [OPTIONS]\n\n{summary}\n\nOptions:");
    if filter_options {
        println!("{FILTER_OPTIONS}");
    }
    if !options.is_empty() {
        println!("{options}");
    }
    println!("{REPO_ROOT_OPTION}\n{SOURCE_SHA_OPTION}");
    println!("  -h, --help                       Print this help");
    if matches!(
        environment,
        CommandEnvironment::Execution | CommandEnvironment::Run
    ) {
        println!("\nEnvironment:\n{PUBLIC_EXECUTION_ENVIRONMENT}");
        if matches!(environment, CommandEnvironment::Run) {
            println!("{RUN_ENVIRONMENT}");
        }
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

/// The repository the manifests, programs and hermit's working directory come
/// from: `--repo-root`, else the checkout this binary was compiled in.
fn root(repo_root: Option<&Path>) -> PathBuf {
    if let Some(root) = repo_root {
        return root.canonicalize().unwrap_or_else(|error| {
            fail(format!(
                "--repo-root {} is unusable: {error}; name an existing Hermit source tree",
                root.display()
            ))
        });
    }
    let compiled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    compiled.canonicalize().unwrap_or_else(|error| {
        fail(format!(
            "the checkout this test-harness was built in, {}, is unusable: {error}; \
             pass --repo-root DIR naming a Hermit source tree",
            compiled.display()
        ))
    })
}

#[derive(Default)]
struct Args {
    selection: Selection,
    prebuilt: bool,
    diagnostic_results: bool,
    allow_empty: bool,
    retries: Retries,
    ci_only: bool,
    probe_disabled: bool,
    results: Option<PathBuf>,
    junit: Option<PathBuf>,
    tpx_json: Option<PathBuf>,
    repo_root: Option<PathBuf>,
    source_sha: Option<String>,
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

/// `--parity-reference` was removed rather than left unknown, so a caller from
/// before <https://github.com/rrnewton/hermit/issues/3301> learns what replaced it.
const REMOVED_PARITY_REFERENCE: &str = "--parity-reference was removed: a ptrace \
     reference run no longer decides a cell's outcome \
     (https://github.com/rrnewton/hermit/issues/3301). Drop the flag; each selected \
     verify cell runs its own backend's strict verification.";

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
            "--label" => {
                for label in required_value(&mut values, "--label").split(',') {
                    if label.is_empty() || args.selection.labels.iter().any(|l| l == label) {
                        fail(format!("--label {label:?} is empty or was given twice"));
                    }
                    args.selection.labels.push(label.to_string());
                }
            }
            "--exclude-backend" => {
                let backend = required_value(&mut values, "--exclude-backend");
                if args.selection.exclude_backends.contains(&backend) {
                    fail(format!("--exclude-backend {backend} was given twice"));
                }
                args.selection.exclude_backends.push(backend);
            }
            "--exclude-category" => {
                let category = required_value(&mut values, "--exclude-category");
                if args.selection.exclude_categories.contains(&category) {
                    fail(format!("--exclude-category {category} was given twice"));
                }
                args.selection.exclude_categories.push(category);
            }
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
            "--parity-reference" => fail(REMOVED_PARITY_REFERENCE),
            "--prebuilt" => args.prebuilt = true,
            "--diagnostic-results" => args.diagnostic_results = true,
            "--allow-empty" => args.allow_empty = true,
            "--no-retry" => args.retries = Retries::Off,
            "--results" => {
                args.results = Some(PathBuf::from(required_value(&mut values, "--results")))
            }
            "--junit" => args.junit = Some(PathBuf::from(required_value(&mut values, "--junit"))),
            "--tpx-json" => {
                args.tpx_json = Some(PathBuf::from(required_value(&mut values, "--tpx-json")))
            }
            "--repo-root" => {
                let value = required_value(&mut values, "--repo-root");
                if args.repo_root.replace(PathBuf::from(value)).is_some() {
                    fail("--repo-root may be specified only once");
                }
            }
            "--source-sha" => set_once(&mut args.source_sha, &mut values, "--source-sha"),
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

/// A node that does not declare diagnostic results writes schema 2, which
/// cannot say a failure is non-blocking. Refuse to run a diagnostic cell there
/// rather than report its failure as an ordinary one. A run that reports to no
/// scheduler (no `DAGRUN_TEST_COUNTS_PATH`, such as one pressure-test sample)
/// may run it: without the declaration its failure is not excused, so it
/// blocks like any other.
fn require_declared_diagnostics(
    cells: &[SelectedCell],
    diagnostic_results: bool,
    reports_to_scheduler: bool,
) -> Result<(), String> {
    if diagnostic_results || !reports_to_scheduler {
        return Ok(());
    }
    match cells.iter().find(|cell| is_diagnostic_cell(cell)) {
        Some(cell) => Err(format!(
            "{} ({}/{}) is a diagnostic cell; only a node that runs with --diagnostic-results may select it",
            cell.id.test,
            cell.id.mode,
            cell.id.backend.as_deref().unwrap_or("native")
        )),
        None => Ok(()),
    }
}

/// The default schema-2 structured report: one terminal row per executed
/// cell, `pass` only for a PASS (an ERROR is a `fail`), with its attempt
/// count. A HOST-INAPPLICABLE cell executed nothing and has no row.
fn terminal_test_results(histories: &[Vec<CellResult>]) -> Result<TestResults, String> {
    let mut rows = Vec::new();
    for history in histories {
        let (result, attempts) = cell_result_and_attempts_after_retries(history)?;
        if result.outcome == "HOST-INAPPLICABLE" {
            continue;
        }
        let id = format!(
            "{} [{}/{}]",
            result.test,
            result.backend.as_deref().unwrap_or("native"),
            result.mode
        );
        rows.push(TestResult::new(id, result.outcome == "PASS", attempts)?);
    }
    TestResults::current(
        u64::try_from(rows.len()).map_err(|_| "cell result count does not fit u64")?,
        0,
        rows,
    )
}

/// The schema-4 structured report dagrun reads for one `--diagnostic-results` run.
///
/// One row per cell that executed (a HOST-INAPPLICABLE cell executed nothing
/// and has no row), named `<test> [<backend>/<mode>]`, with every attempt's
/// classified cause. A diagnostic cell's product failure is a
/// `diagnostic_fail` row carrying the manifest's reason, so dagrun reports it
/// without failing the node; every other failure is a blocking `fail`.
fn structured_test_results(histories: &[Vec<CellResult>]) -> Result<TestResults, String> {
    let mut rows = Vec::new();
    for history in histories {
        let (result, _) = cell_result_and_attempts_after_retries(history)?;
        if result.outcome == "HOST-INAPPLICABLE" {
            continue;
        }
        let id = format!(
            "{} [{}/{}]",
            result.test,
            result.backend.as_deref().unwrap_or("native"),
            result.mode
        );
        let attempts = history
            .iter()
            .map(structured_attempt)
            .collect::<Result<Vec<_>, _>>()?;
        rows.push(match diagnostic_failure_reason(history) {
            Some(reason) => TestResult::diagnostic_failure(id, attempts, reason.to_string())?,
            None => TestResult::with_attempt_results(id, result.outcome == "PASS", attempts)?,
        });
    }
    TestResults::diagnostic(
        u64::try_from(rows.len()).map_err(|_| "cell result count does not fit u64")?,
        0,
        rows,
    )
}

/// The reason a finished cell is an excused diagnostic failure: the run
/// declared `--diagnostic-results`, the row the harness reports for the cell is
/// a FAIL, and its history ends in a diagnostic cell's measured failure. A cell
/// whose history could not be summarized is reported as an ERROR and is never
/// excused, and an undeclared run excuses nothing.
fn excused_diagnostic<'a>(
    declared: bool,
    summarized: &CellResult,
    history: &'a [CellResult],
) -> Option<&'a str> {
    if !declared || summarized.outcome != "FAIL" {
        return None;
    }
    diagnostic_failure_reason(history)
}

/// One attempt's classified cause, from the row that attempt published.
///
/// The runner's own typed fields decide it: its `cpu-timeout` and
/// `wall-timeout` error kinds are the two timeouts; otherwise the
/// producer-owned failure class, where `no_result` stays `no_result` and an
/// understood infrastructure or prerequisite failure is
/// `infrastructure_error`; a FAIL with a product failure is `failed`. The
/// detail is the row's own reason, never an invented one.
fn structured_attempt(result: &CellResult) -> Result<TestAttemptResult, String> {
    let outcome = match (
        result.outcome.as_str(),
        result.error_kind.as_deref(),
        result.failure_class,
    ) {
        ("PASS", _, _) => {
            return TestAttemptResult::new(result.attempt, TestAttemptOutcome::Passed, None);
        }
        ("FAIL", Some("cpu-timeout"), _) => TestAttemptOutcome::CpuTimeout,
        ("FAIL", Some("wall-timeout"), _) => TestAttemptOutcome::WallTimeout,
        ("FAIL" | "ERROR", _, Some(FailureClass::NoResult)) => TestAttemptOutcome::NoResult,
        (
            "FAIL" | "ERROR",
            _,
            Some(
                FailureClass::UnderstoodInfrastructureFailure
                | FailureClass::UnderstoodPrerequisiteFailure,
            ),
        )
        | ("ERROR", _, Some(FailureClass::ProductFailure) | None) => {
            TestAttemptOutcome::InfrastructureError
        }
        ("FAIL", _, Some(FailureClass::ProductFailure) | None) => TestAttemptOutcome::Failed,
        (other, _, _) => {
            return Err(format!(
                "{} ({}) attempt {} has outcome {other}, which has no structured attempt cause",
                result.test, result.mode, result.attempt
            ));
        }
    };
    let detail = result.reason_for_display().trim();
    let detail = if detail.is_empty() {
        format!("{} with an empty recorded reason", result.outcome)
    } else {
        detail.to_string()
    };
    TestAttemptResult::new(result.attempt, outcome, Some(detail))
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

/// [`host_inapplicable_reason`] for `cell`. The hermit binary's
/// `exact_branch_counter` verdict counts only for a cell the binary refuses on
/// an inexact counter (see [`refused_on_an_inexact_branch_counter`]); a custom
/// cell without `--strict` still runs.
fn cell_host_inapplicable_reason(
    cell: &SelectedCell,
    verdicts: &HostCapabilities,
    exact_branch_counter: Option<&HostCapabilityVerdict>,
) -> Option<(Vec<String>, String)> {
    host_inapplicable_reason(
        &cell.test.requires,
        cell.id.backend.as_deref(),
        verdicts,
        exact_branch_counter.filter(|_| refused_on_an_inexact_branch_counter(cell)),
    )
}

/// Why a cell cannot run on this machine, if it cannot: a capability its
/// `requires` tokens or its backend need is proven absent, or the hermit
/// binary's `exact_branch_counter` verdict, passed only for a cell the binary
/// refuses without it, is absent. `None` withholds nothing.
fn host_inapplicable_reason(
    requires: &[String],
    backend: Option<&str>,
    verdicts: &HostCapabilities,
    exact_branch_counter: Option<&HostCapabilityVerdict>,
) -> Option<(Vec<String>, String)> {
    let mut absent = requires
        .iter()
        .filter_map(|token| requires_capability(token).ok().flatten())
        .chain(backend.and_then(backend_capability))
        .filter_map(|capability| {
            verdicts
                .get(&capability)
                .filter(|verdict| !verdict.present)
                .map(|verdict| (capability.value().to_string(), verdict.evidence.clone()))
        })
        .chain(
            exact_branch_counter
                .filter(|verdict| !verdict.present)
                .map(|verdict| ("exact-branch-counter".to_string(), verdict.evidence.clone())),
        )
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

/// Apply an explicit top-level audit width without changing each child's
/// admitted internal build width.
///
/// Each child still inherits `CARGO_BUILD_JOBS`, so a child may use every core
/// admitted to the gate for its own Cargo and helper work. Starting a second
/// top-level audit does not create more admitted CPU; it makes the two large
/// self-tests contend for caches while charging the same whole-step CPU cap.
/// RUN1909 exposed the missing argv override before any audit ran. With that
/// fixed, the exact scheduler path then consumed 601.132 CPU seconds and was
/// killed by the unchanged 600-second cap, while isolated controls consumed
/// 327.344 seconds for scorecard and 70.699 seconds for pressure. The ordinary
/// gate explicitly selects one top-level worker to remove that cross-audit
/// contention. An unaffected prebuilt gate retains the existing two-worker
/// default; unavailable prebuilt scripts and malformed explicit values fail
/// closed to serial execution.
///
/// Since 2026-09-29 the five tool self-tests run as their own `selftest.<name>`
/// DAG nodes (<https://github.com/rrnewton/hermit/issues/3381>), so `validate`
/// schedules only `generate-test-footprints --check` here and this width no
/// longer changes what the gate spends. The history above explains the value
/// and is kept for the audits that may be added back.
fn validation_audit_worker_capacity(
    prebuilt_rust_scripts: bool,
    configured_jobs: Option<&str>,
) -> ScheduledWorkerCapacity {
    if !prebuilt_rust_scripts {
        return ScheduledWorkerCapacity::new(1);
    }
    let jobs = match configured_jobs {
        None => DEFAULT_VALIDATE_AUDIT_JOBS,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .map(|value| value.min(DEFAULT_VALIDATE_AUDIT_JOBS))
            .unwrap_or(1),
    };
    ScheduledWorkerCapacity::new(jobs)
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
    if args.selection.backend.as_deref().is_some_and(|backend| {
        !matches!(
            backend,
            "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst" | "in-guest-trap"
        )
    }) {
        fail("--backend must name a Hermit backend");
    }
    if args.selection.exclude_backends.iter().any(|backend| {
        !matches!(
            backend.as_str(),
            "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst" | "in-guest-trap"
        )
    }) {
        fail("--exclude-backend must name a Hermit backend");
    }
    if let Some(backend) = args.selection.backend.as_deref() {
        if args
            .selection
            .exclude_backends
            .iter()
            .any(|excluded| excluded == backend)
        {
            fail(format!(
                "--backend {backend} and --exclude-backend {backend} select nothing; name one"
            ));
        }
    }
    if command == "build" && args.prebuilt {
        fail("build does not accept --prebuilt");
    }
    if command != "run" && args.diagnostic_results {
        fail("--diagnostic-results is accepted by run only");
    }
    if command != "run" && args.retries == Retries::Off {
        fail("--no-retry is accepted by run only");
    }
    if command != "run" && args.tpx_json.is_some() {
        fail("--tpx-json is accepted by run only");
    }
    if let Some(sha) = args.source_sha.as_deref() {
        let Some(repo_root) = args.repo_root.as_deref() else {
            fail(
                "--source-sha describes a --repo-root source snapshot; pass --repo-root DIR \
                 naming the `git archive` of that commit",
            );
        };
        validate_source_snapshot(repo_root, sha).unwrap_or_else(|error| fail(error));
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
        if !args.selection.exclude_backends.is_empty() {
            fail(
                "--probe-disabled names one exact backend; --exclude-backend has no meaning there",
            );
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
        println!("{HELP}\n\nEnvironment:\n{PUBLIC_EXECUTION_ENVIRONMENT}\n{RUN_ENVIRONMENT}");
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
    if command == "sync-cells" {
        return sync_cells(&values);
    }
    if command == "parity" {
        let request = match parse_parity(values) {
            ParityRequest::Compare(request) => request,
            ParityRequest::Export(request) => return parity_export(&request),
        };
        let root = root(None);
        let manifests = ManifestSet::load(&root).unwrap_or_else(|error| fail(error));
        run_manifest_plan(&root, None);
        return parity_compare(&root, &manifests, &request);
    }
    if command == "selftest" {
        return run_tool_self_test(&values);
    }
    if command == "expected-plan"
        && !values.chunks(2).all(
            |pair| matches!(pair, [flag, _] if flag == "--repo-root" || flag == "--source-sha"),
        )
    {
        fail("expected-plan accepts only --repo-root DIR and --source-sha SHA");
    }
    let args = parse(values.into_iter());
    validate_args(&command, &args);
    let root = root(args.repo_root.as_deref());
    let manifests = ManifestSet::load(&root).unwrap_or_else(|error| fail(error));
    // One front-door schema/inventory authority governs every command, not
    // only the metadata gate. This prevents a direct/manual run from accepting
    // a recipe that the canonical manifest planner would refuse.
    run_manifest_plan(&root, args.source_sha.as_deref());
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
    // Only the generated-footprint check remains here: it compares committed
    // generated metadata with its source, which is what this gate owns. The
    // repository tools' own self-tests (TOOL_SELF_TESTS) are not preconditions
    // of any product node, so each runs as its own `selftest.<name>` leaf node
    // through `test-harness selftest <name>`; nothing depends on those nodes,
    // and a failure still makes the validation red. Publish the child's output
    // on completion so a timeout retains its diagnostics.
    let audit_jobs = validation_audit_worker_capacity(
        std::env::var(PREBUILT_RUST_SCRIPTS_REQUIRED).as_deref() == Ok("1"),
        std::env::var(VALIDATE_AUDIT_JOBS_ENV).ok().as_deref(),
    )
    .configured();
    run_audits_parallel(
        root,
        &[(
            root.join("target/debug/generate-test-footprints"),
            vec!["--check"],
        )],
        audit_jobs,
    );
    audit_cli_brackets(root);
    let cells = audit_expected_plan(root, manifests);
    // Byte for byte, so a hand edit that `sync-cells` would undo is refused
    // here rather than at the next flip.
    let synced = synced_cell_files(root).unwrap_or_else(|error| fail(error));
    let stale = stale_files(root, &synced);
    if !stale.is_empty() {
        fail(format!(
            "not what the manifests derive: {}; run ci/sync-cell-config.sh",
            stale.join(", ")
        ));
    }
    println!(
        "PASS: {} YAML manifests, {} required cells",
        manifests.documents.len(),
        cells
    );
    ExitCode::SUCCESS
}

fn selftest_help() -> String {
    let mut help = String::from(
        "Usage: test-harness selftest <NAME>\n\n\
         Run one repository tool's self-test from the repository root. The\n\
         validation DAG runs each as its own selftest.<NAME> node.\n\nNames:",
    );
    for tool in TOOL_SELF_TESTS {
        help.push_str(&format!(
            "\n  {:<32} {}",
            tool.name,
            tool_self_test_command(tool)
        ));
    }
    help.push_str("\n\nOptions:\n  -h, --help                       Print this help");
    help
}

fn tool_self_test_command(tool: &ToolSelfTest) -> String {
    std::iter::once(tool.program)
        .chain(tool.args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_tool_self_test(values: &[String]) -> ExitCode {
    let names = TOOL_SELF_TESTS
        .iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>()
        .join(", ");
    let [name] = values else {
        fail(format!("selftest takes exactly one name, one of: {names}"));
    };
    let tool = TOOL_SELF_TESTS
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| {
            fail(format!(
                "unknown self-test {name}; expected one of: {names}"
            ))
        });
    let root = root(None);
    let started = std::time::Instant::now();
    if let Some(triggers) = tool.run_when_changed {
        // The NOT RUN message is one line and the node's last, so the
        // scheduler keeps it as the node's summary.
        let selection = std::env::var(self_test_selection::SELECTION_ENV).ok();
        match self_test_selection::decide(
            selection.as_deref(),
            self_test_selection::change_set(&root),
            triggers,
        ) {
            self_test_selection::Decision::Skip { base, changed } => {
                println!(
                    "{}",
                    self_test_selection::not_run_line(tool.name, &base, &changed, triggers)
                );
                return ExitCode::SUCCESS;
            }
            self_test_selection::Decision::Run(reason) => println!(
                "test-harness: self-test {} selected ({:.3}s): {reason}",
                tool.name,
                started.elapsed().as_secs_f64()
            ),
        }
    }
    // Hand the helper this build wrote beside us to a program that would
    // otherwise build it with Cargo inside the node's CPU cap (see
    // MANIFEST_PLAN_BIN_ENV in ci/compat-envelope/scorecard.rs).
    let mut envs = Vec::new();
    if tool.manifest_plan_helper {
        let helper = sibling_manifest_plan()
            .filter(|helper| helper.is_file())
            .unwrap_or_else(|| {
                fail(format!(
                    "self-test {} needs the hermit-manifest-plan binary built beside \
                     test-harness; build both with `cargo build -p hermit-manifest-plan --bins`",
                    tool.name
                ))
            });
        envs.push((MANIFEST_PLAN_BIN_ENV, helper));
    }
    run_audit_with_env(&root, &root.join(tool.program), tool.args, &envs);
    println!(
        "test-harness: self-test {} passed: {}; elapsed={:.3}s",
        tool.name,
        tool_self_test_command(tool),
        started.elapsed().as_secs_f64()
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
        "--exclude-backend",
        "--label",
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

/// Run the canonical manifest planner over `root`. A `source_sha` means the
/// root is a source snapshot, so the planner takes the test population from the
/// files present instead of from Git.
fn run_manifest_plan(root: &Path, source_sha: Option<&str>) {
    let manifest_plan =
        sibling_manifest_plan().unwrap_or_else(|| root.join("target/debug/hermit-manifest-plan"));
    let mut command = Command::new(&manifest_plan);
    command.args(["--format", "json", "--root"]).arg(root);
    if source_sha.is_some() {
        command.arg("--source-snapshot");
    }
    let status = command
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
                // The backend's own capability withholds the cell exactly as a
                // `requires` token does (see `host_inapplicable_reason`), so a
                // consumer routing from this file must see it too.
                .chain(cell.id.backend.as_deref().and_then(backend_capability))
                .collect::<BTreeSet<_>>();
            // `requires` and the base (unmultiplied) timeouts let a consumer that
            // cannot load the manifests, such as a Buck cell generator, route
            // and budget each cell from this file alone.
            let mut row = serde_json::json!({
                "test": cell.id.test,
                "category": cell.category,
                "lane": cell.test.lane,
                "mode": cell.id.mode,
                "backend": cell.id.backend,
                "requires": cell.test.requires,
                "timeout_seconds": cell.timeout_seconds,
                "cpu_timeout_seconds": cell.cpu_timeout_seconds,
                "classification": if is_diagnostic_cell(cell) { "diagnostic" } else { "required" },
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
        fail(
            "required E2E plan changed; regenerate ci/expected-e2e-plan.json with \
             ci/sync-cell-config.sh and commit it in the same review",
        );
    }
    cell_count
}

/// Every enabled cell that is not required (a `ci: false` cell), one
/// `<test> <mode> <backend>` line each, sorted. No other derived file names
/// these cells, so this inventory is what makes disabling one show up as a
/// diff (<https://github.com/rrnewton/hermit/issues/3606>).
fn optional_cells_document(manifests: &ManifestSet) -> Result<String, String> {
    let select = |population| {
        manifests.select(&Selection {
            population: Some(population),
            ..Selection::default()
        })
    };
    let required = select(Population::Required)?
        .into_iter()
        .map(|cell| cell.id)
        .collect::<BTreeSet<_>>();
    let lines = select(Population::Enabled)?
        .into_iter()
        .filter(|cell| !required.contains(&cell.id))
        .map(|cell| {
            format!(
                "{} {} {}\n",
                cell.id.test,
                cell.id.mode,
                cell.id.backend.as_deref().unwrap_or("-")
            )
        })
        .collect::<BTreeSet<_>>();
    Ok(format!(
        "# Enabled E2E cells that are not required (ci: false): <test> <mode> <backend>.\n\
         # Generated from tests/e2e/manifests by ci/sync-cell-config.sh; do not edit.\n{}",
        lines.into_iter().collect::<String>()
    ))
}

/// The files `sync-cells` derives, each with the text the manifests now give
/// it, in the order it reports them.
fn synced_cell_files(root: &Path) -> Result<Vec<(&'static str, String)>, String> {
    let manifests = ManifestSet::load(root)?;
    let generated = expected_plan_document(root, &manifests);
    // `expected-plan` prints the same document with println!.
    let plan = serde_json::to_string_pretty(&generated).map_err(|error| error.to_string())? + "\n";
    Ok(vec![
        (EXPECTED_PLAN_PATH, plan),
        (OPTIONAL_CELLS_PATH, optional_cells_document(&manifests)?),
    ])
}

/// The files whose committed bytes differ from `files`.
fn stale_files(root: &Path, files: &[(&'static str, String)]) -> Vec<&'static str> {
    files
        .iter()
        .filter(|(relative, text)| {
            fs::read(root.join(relative)).ok().as_deref() != Some(text.as_bytes())
        })
        .map(|(relative, _)| *relative)
        .collect()
}

fn sync_cells(values: &[String]) -> ExitCode {
    let mut write = None;
    let mut repo_root = None;
    let mut values = values.iter();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--check" | "--write" if write.is_none() => write = Some(flag == "--write"),
            "--check" | "--write" => fail("sync-cells takes one of --check and --write, once"),
            "--repo-root" => {
                let dir = values
                    .next()
                    .unwrap_or_else(|| fail("--repo-root needs a directory"));
                repo_root = Some(PathBuf::from(dir));
            }
            other => fail(format!(
                "sync-cells does not accept {other}; see `test-harness sync-cells --help`"
            )),
        }
    }
    let Some(write) = write else {
        fail("sync-cells needs --check or --write; see `test-harness sync-cells --help`")
    };
    let root = root(repo_root.as_deref());
    let files = synced_cell_files(&root)
        .unwrap_or_else(|error| fail(format!("sync-cells: {error}; nothing was written")));
    let stale = stale_files(&root, &files);
    if write {
        for (relative, text) in files.iter().filter(|(path, _)| stale.contains(path)) {
            fs::write(root.join(relative), text)
                .unwrap_or_else(|error| fail(format!("cannot write {relative}: {error}")));
        }
    }
    match (stale.is_empty(), write) {
        (true, _) => println!("sync-cells: the expected plan and optional cells are current"),
        (false, true) => println!("sync-cells: wrote {}", stale.join(", ")),
        (false, false) => {
            eprintln!(
                "sync-cells: stale: {}; run ci/sync-cell-config.sh to regenerate them",
                stale.join(", ")
            );
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// Read by ci/compat-envelope/scorecard.rs: a prepared `hermit-manifest-plan`
/// binary to run instead of `cargo run`.
const MANIFEST_PLAN_BIN_ENV: &str = "HERMIT_MANIFEST_PLAN_BIN";

/// The `hermit-manifest-plan` binary beside this executable.
fn sibling_manifest_plan() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|path| {
        path.parent()
            .map(|parent| parent.join("hermit-manifest-plan"))
    })
}

fn run_audit(root: &Path, program: &Path, args: &[&str]) {
    run_audit_with_env(root, program, args, &[]);
}

fn run_audit_with_env(root: &Path, program: &Path, args: &[&str], envs: &[(&str, PathBuf)]) {
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().map(|(name, value)| (name, value)))
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

struct AuditReport {
    status: Result<std::process::ExitStatus, String>,
    elapsed: std::time::Duration,
}

enum AuditEvent {
    Started,
    Finished(Result<Output, String>, std::time::Duration),
}

/// Publish each completed audit before waiting for the remaining workers.
/// A later timeout must not erase an earlier audit's output or actual status.
fn collect_audit_results(
    root: &Path,
    audits: &[(PathBuf, Vec<&str>)],
    jobs: usize,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<Vec<AuditReport>, String> {
    let mut results = std::iter::repeat_with(|| None)
        .take(audits.len())
        .collect::<Vec<Option<AuditReport>>>();
    let mut output_error = None;
    for_each_parallel(
        audits.len(),
        ScheduledWorkerCapacity::new(jobs),
        |index, emit| {
            if !emit(AuditEvent::Started, false) {
                return;
            }
            let (program, args) = &audits[index];
            let started = std::time::Instant::now();
            let result = Command::new(program)
                .args(args)
                .current_dir(root)
                .output()
                .map_err(|error| format!("cannot execute {}: {error}", program.display()));
            let _ = emit(AuditEvent::Finished(result, started.elapsed()), false);
        },
        |index, event, _| {
            if output_error.is_some() {
                return false;
            }
            let (program, args) = &audits[index];
            let replay = (|| -> std::io::Result<()> {
                match event {
                    AuditEvent::Started => writeln!(
                        stderr,
                        "test-harness: audit {}/{} START {} {}",
                        index + 1,
                        audits.len(),
                        program.display(),
                        args.join(" ")
                    )?,
                    AuditEvent::Finished(result, elapsed) => {
                        if let Ok(output) = &result {
                            stdout.write_all(&output.stdout)?;
                            stderr.write_all(&output.stderr)?;
                        }
                        let status = result.map(|output| output.status);
                        let detail = match &status {
                            Ok(status) => status.to_string(),
                            Err(error) => error.clone(),
                        };
                        writeln!(
                            stderr,
                            "test-harness: audit {}/{} END {} {}: {detail}; elapsed={:.3}s",
                            index + 1,
                            audits.len(),
                            program.display(),
                            args.join(" "),
                            elapsed.as_secs_f64()
                        )?;
                        results[index] = Some(AuditReport { status, elapsed });
                    }
                }
                stdout.flush()?;
                stderr.flush()
            })();
            if let Err(error) = replay {
                output_error = Some(format!(
                    "cannot publish audit {} diagnostics: {error}",
                    program.display()
                ));
                return false;
            }
            true
        },
    );
    if let Some(error) = output_error {
        return Err(error);
    }
    Ok(results
        .into_iter()
        .map(|result| result.expect("every validation audit worker returns one result"))
        .collect())
}

fn run_audits_parallel(root: &Path, audits: &[(PathBuf, Vec<&str>)], jobs: usize) {
    let results = collect_audit_results(
        root,
        audits,
        jobs,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
    .unwrap_or_else(|error| fail(error));

    // Output bodies were published exactly once at completion. Keep this small
    // terminal summary in the declared order, independent of completion order.
    for ((program, args), result) in audits.iter().zip(&results) {
        let status = match &result.status {
            Ok(status) => status.to_string(),
            Err(error) => error.clone(),
        };
        println!(
            "test-harness: audit result {} {}: {status}; elapsed={:.3}s",
            program.display(),
            args.join(" "),
            result.elapsed.as_secs_f64()
        );
    }
    for ((program, args), result) in audits.iter().zip(results) {
        let status = result.status.unwrap_or_else(|error| fail(error));
        if !status.success() {
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
    let current_separator = format!(
        " -- bash -c {} bash ",
        shell_quote_one(PINNED_ROOT_COMMAND_GUARD)
    );
    let Some((forwarded, quoted_inner)) = rest
        .split_once(&current_separator)
        .or_else(|| rest.split_once(PINNED_COMMAND_SEPARATOR))
    else {
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
            // c-programs omits --allow-empty so an empty selection fails
            // (https://github.com/rrnewton/hermit/issues/3301, slice S6); the
            // generator and this audit read the same flag list.
            let selector = format!(
                "target/debug/test-harness run --lane {lane} --category {category} {}",
                hermit_manifest_plan::validation_dag::manifest_selector_flags(category)
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

// Match the actual hosted selector and shard-coverage consumer: an exact name
// wins; only an existing hosted counterpart can resolve a public selector.
// Keep the public name in the budget baseline and inspect the resolved node's
// real timeout. Unknown or removed nodes remain errors.
fn portable_shard_step<'a>(
    steps: &std::collections::BTreeMap<String, &'a dagrun::Step>,
    node: &str,
) -> Result<&'a dagrun::Step, String> {
    steps
        .get(node)
        .or_else(|| steps.get(&format!("{node}_on_host")))
        .copied()
        .ok_or_else(|| format!("portable shard names missing DAG node {node}"))
}

// The same exact-name-first resolution for the preflight and check job
// populations, which ci/run-node.sh hands to validate's hosted selector
// (map_hosted_portable_tags in scripts/validate.rs). A name with neither an
// exact node nor a hosted twin is kept, so the selection still rejects it.
fn resolve_portable_selectors(portable: &dagrun::DagConfig, nodes: Vec<String>) -> Vec<String> {
    let available = portable
        .steps
        .iter()
        .map(dagrun::Step::tag)
        .collect::<BTreeSet<_>>();
    nodes
        .into_iter()
        .map(|node| {
            let hosted = format!("{node}_on_host");
            if !available.contains(&node) && available.contains(&hosted) {
                hosted
            } else {
                node
            }
        })
        .collect()
}

// 4200 = 3780 plus the 120 seconds setup.manifest_plan's wall cap grew (180 to
// 300) in https://github.com/rrnewton/hermit/issues/3381, plus the 300 seconds
// build.rust_scripts' wall cap grew (900 to 1200) to keep 1.5 times its
// largest observed wall.
const PORTABLE_PREFLIGHT_CRITICAL_PATH_SECONDS: u64 = 4200;
const PORTABLE_PREFLIGHT_OVERHEAD_SECONDS: u64 = 420;
const PORTABLE_CHECKS_CRITICAL_PATH_SECONDS: u64 = 2400;
const PORTABLE_CHECKS_OVERHEAD_SECONDS: u64 = 600;
const PRIVILEGED_WORKFLOW_OVERHEAD_SECONDS: u64 = 300;

fn audit_portable_preflight_budget(
    workflow: &YamlValue,
    portable: &dagrun::DagConfig,
    shards: &JsonValue,
) -> Result<(), String> {
    let preflight_nodes = shards["preflight_nodes"]
        .as_array()
        .ok_or_else(|| "portable shard map has no preflight_nodes array".to_string())?
        .iter()
        .map(|node| {
            node.as_str()
                .map(str::to_string)
                .ok_or_else(|| "portable preflight_nodes contains a non-string node".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let preflight_nodes = resolve_portable_selectors(portable, preflight_nodes);
    // ci/run-node.sh passes --ignore-selected-deps: dependencies outside the
    // declared preflight population are supplied by the workflow, while edges
    // among these five selected nodes remain load-bearing.
    let selected = dagrun::select_steps_by_tags(portable, &preflight_nodes, true)
        .map_err(|error| format!("cannot select portable preflight closure: {error}"))?;
    let critical_path = dag_critical_path(&selected)?;
    if critical_path != PORTABLE_PREFLIGHT_CRITICAL_PATH_SECONDS {
        return Err(format!(
            "portable preflight critical path changed from {PORTABLE_PREFLIGHT_CRITICAL_PATH_SECONDS}s to {critical_path}s"
        ));
    }
    let job_bound = workflow_job_timeout(workflow, "preflight")? * 60;
    let required = critical_path
        .checked_add(PORTABLE_PREFLIGHT_OVERHEAD_SECONDS)
        .ok_or_else(|| "portable preflight required budget overflowed".to_string())?;
    if job_bound < required {
        return Err(format!(
            "portable preflight job {job_bound}s must cover its {critical_path}s constructed DAG critical path plus at least {PORTABLE_PREFLIGHT_OVERHEAD_SECONDS}s for checkout, package installation, artifact transfer, and teardown"
        ));
    }
    Ok(())
}

fn audit_portable_checks_budget(
    workflow: &YamlValue,
    portable: &dagrun::DagConfig,
    shards: &JsonValue,
) -> Result<(), String> {
    let check_nodes = shards["check_nodes"]
        .as_array()
        .ok_or_else(|| "portable shard map has no check_nodes array".to_string())?
        .iter()
        .map(|node| {
            node.as_str()
                .map(str::to_string)
                .ok_or_else(|| "portable check_nodes contains a non-string node".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let check_nodes = resolve_portable_selectors(portable, check_nodes);
    let selected = dagrun::select_steps_by_tags(portable, &check_nodes, true)
        .map_err(|error| format!("cannot select portable checks closure: {error}"))?;
    let critical_path = dag_critical_path(&selected)?;
    if critical_path != PORTABLE_CHECKS_CRITICAL_PATH_SECONDS {
        return Err(format!(
            "portable checks critical path changed from {PORTABLE_CHECKS_CRITICAL_PATH_SECONDS}s to {critical_path}s"
        ));
    }
    let job_bound = workflow_job_timeout(workflow, "checks")? * 60;
    let required = critical_path
        .checked_add(PORTABLE_CHECKS_OVERHEAD_SECONDS)
        .ok_or_else(|| "portable checks required budget overflowed".to_string())?;
    if job_bound < required {
        return Err(format!(
            "portable checks job {job_bound}s must cover its {critical_path}s constructed DAG critical path plus at least {PORTABLE_CHECKS_OVERHEAD_SECONDS}s for checkout, package installation, artifact transfer, and teardown"
        ));
    }
    Ok(())
}

fn audit_portable_reducer_prepared_tools(workflow: &YamlValue) -> Result<(), String> {
    let steps = workflow["jobs"]["regular"]["steps"]
        .as_sequence()
        .ok_or_else(|| "portable regular reducer has no steps sequence".to_string())?;
    let download = steps.iter().position(|step| {
        step["uses"]
            .as_str()
            .is_some_and(|uses| uses.starts_with("actions/download-artifact@"))
            && step["with"]["name"].as_str() == Some("${{ env.MANIFEST_PLAN_ARTIFACT }}")
            && step["if"].as_str() == Some("needs.select.outputs.run_e2e != 'false'")
    });
    let unpack = steps.iter().position(|step| {
        step["run"].as_str().is_some_and(|run| {
            run.contains("tar -xzf \"$MANIFEST_PLAN_TARBALL\"")
                && run.contains("./ci/prepare-rust-scripts.sh --check")
        }) && step["if"].as_str() == Some("needs.select.outputs.run_e2e != 'false'")
    });
    let verdict = steps.iter().position(|step| {
        step["run"]
            .as_str()
            .is_some_and(|run| run.contains("./ci/run-node.sh portable"))
    });
    match (download, unpack, verdict) {
        (Some(download), Some(unpack), Some(verdict)) if download < unpack && unpack < verdict => {
            Ok(())
        }
        _ => Err("portable regular reducer must download and verify the prepared rust-script artifact before its constructed scorecard node".into()),
    }
}

fn audit_privileged_workflow_overhead(workflow: &YamlValue) -> Result<(), String> {
    let job_bound = workflow_job_timeout(workflow, "privileged")? * 60;
    let declared_step_budgets = workflow_step_timeout_sum(workflow, "privileged")?;
    let required = declared_step_budgets
        .checked_add(PRIVILEGED_WORKFLOW_OVERHEAD_SECONDS)
        .ok_or_else(|| "privileged workflow required budget overflowed".to_string())?;
    if job_bound < required {
        return Err(format!(
            "privileged job {job_bound}s must cover {declared_step_budgets}s of explicit inner step budgets plus at least {PRIVILEGED_WORKFLOW_OVERHEAD_SECONDS}s for setup, checkout, artifact transfer, and teardown"
        ));
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
    audit_portable_preflight_budget(&portable_workflow, &portable, &shards)?;
    audit_portable_checks_budget(&portable_workflow, &portable, &shards)?;
    audit_portable_reducer_prepared_tools(&portable_workflow)?;
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
            let step = portable_shard_step(&portable_steps, node)?;
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
    audit_privileged_workflow_overhead(&privileged_yaml)?;
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
    // Both launches are pinned: the boxing-capable one whenever the step's
    // environment carries any cgroup input the runner reads, and the explicit
    // unboxed one only when it carries none.
    const VALIDATION_PRIVILEGED: &str = "if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n  echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n  exit 2\nfi\nif [[ -n ${DAGRUN_DELEGATED_CGROUP+set}${DAGRUN_IN_SCOPE+set}${DAGRUN_SCOPE_UNIT+set}${DAGRUN_DIRECT_CGROUP+set}${DAGRUN_DELEGATED_UNBOXED+set}${DAGRUN_FORCE_SCOPE_ATTEMPT+set} ]]; then\n  env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v\nelse\n  env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --unsafe-no-cgroups --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v\nfi";
    const STANDALONE_PRIVILEGED: &str = "if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n  echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n  exit 2\nfi\ntimeout --foreground --kill-after=10s 2460s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --unsafe-no-cgroups --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v";
    const FIXTURES: &[&str] = &[
        "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh portable -v",
        "timeout --foreground --kill-after=10s 2160s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -v",
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
    let context = RunContext::from_env(root.to_path_buf(), false, args.source_sha.as_deref())
        .unwrap_or_else(|e| fail(e));
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
    let context = RunContext::from_env(root.to_path_buf(), false, args.source_sha.as_deref())
        .unwrap_or_else(|e| fail(e));
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
            host_level: hermit_manifest_plan::runner::HostLevel::default(),
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

/// Whether `run` may retry a product failure at all.
///
/// `Off` is the flake-measurement setting (`--no-retry`): every selected cell
/// runs exactly once, so N repeated runs yield N first-attempt observations
/// instead of a pass rate that the retry has already rounded up. It changes
/// only whether a retry is launched; each row's outcome and failure class are
/// unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Retries {
    #[default]
    Framework,
    Off,
}

/// Why a finished attempt is retried.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetryCause {
    /// A measured product failure ([`cell_result_is_retryable`]).
    ProductFailure,
    /// A verify cell whose only failure is Hermit's typed precise-timer
    /// overshoot ([`skid_overshoot_only_reports`]), carrying the number of
    /// `HERMIT_SKID_OVERSHOOT` reports. It is counted and reported as a
    /// SKID-RETRY (<https://github.com/rrnewton/hermit/issues/1845>).
    SkidOvershoot { reports: u64 },
    /// A verify cell whose only failure is a divergence that a host file
    /// changing during one run explains ([`host_input_change_only`]). It is
    /// retried even when the cell declares `no_retry_reason`, which keeps
    /// only a product failure final, and reported as a HOST-INPUT-RETRY.
    HostInputChanged,
}

/// Why this finished attempt of `cell` is retried, or `None` when it is not.
///
/// Retries apply only in a run that has not turned them off, and only to a
/// cell that has not declared `no_retry_reason`, so `--no-retry` measurements
/// and the single-attempt compatibility corpus keep exactly one attempt. The
/// shared [`MAX_ATTEMPTS_PER_CELL`] caps both causes together. Every other
/// failure, including a divergence, timeout or no-result that coincides with
/// an overshoot report, has no cause and is final.
fn attempt_retry_cause(
    retries: Retries,
    cell: &SelectedCell,
    result: &CellResult,
) -> Option<RetryCause> {
    if retries != Retries::Framework {
        return None;
    }
    // The host-input and skid rules check the stdout assertions the row
    // records, and the skid rule also admits the guest disposition and applies
    // the comparator it records, so the row must record exactly what this
    // cell's manifest declares; an imported row recorded under another
    // declaration or recipe does not choose its own. An executed row either
    // rule can admit always records its cell's declarations.
    let records_this_cell = || {
        result.expected_guest_exit == cell_expected_guest_exit(cell)
            && result.declared_stdout.as_ref() == Some(&cell_declared_stdout(cell))
            && result.relaxations == cell_relaxations(cell)
    };
    if host_input_change_only(result) && records_this_cell() {
        return Some(RetryCause::HostInputChanged);
    }
    if !retries_product_failures(cell) {
        return None;
    }
    if cell_result_is_retryable(result.outcome.as_str(), result.failure_class) {
        return Some(RetryCause::ProductFailure);
    }
    if !records_this_cell() {
        return None;
    }
    skid_overshoot_only_reports(result).map(|reports| RetryCause::SkidOvershoot { reports })
}

/// Whether this finished attempt of `cell` is retried; see
/// [`attempt_retry_cause`].
fn attempt_earns_retry(retries: Retries, cell: &SelectedCell, result: &CellResult) -> bool {
    attempt_retry_cause(retries, cell, result).is_some()
}

/// One SKID-RETRY of a cell's history: the retried attempt, its overshoot
/// report count, and the outcome the cell's complete history selected.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SkidRetry {
    test: String,
    mode: String,
    backend: Option<String>,
    attempt: u64,
    reports: u64,
    final_outcome: String,
}

/// One retried attempt of a cell's history, whatever its cause, and the
/// outcome the cell's complete history selected.
#[derive(Clone, Debug)]
struct RetriedAttempt<'a> {
    row: &'a CellResult,
    cause: RetryCause,
    final_outcome: String,
}

/// Every retried attempt in `histories`, with its cause.
///
/// Every row but the last of a history is an attempt the harness retried (or,
/// for imported rows, one this run's policy would have retried), so the cause
/// is recomputed from that row with the same rule that launched the retry. An
/// imported history continues past an attempt exactly when that rule retries
/// it ([`imported_results`]), so an imported retried attempt followed by its
/// producer's next attempt is counted as an executed one is.
fn retried_attempts<'a>(
    retries: Retries,
    cells: &[SelectedCell],
    histories: &'a [Vec<CellResult>],
) -> Vec<RetriedAttempt<'a>> {
    let mut found = Vec::new();
    for (cell, history) in cells.iter().zip(histories) {
        let Some((_, retried)) = history.split_last() else {
            continue;
        };
        let final_outcome = match cell_result_after_retries(history) {
            Ok(result) => result.outcome.clone(),
            Err(_) => "ERROR".into(),
        };
        for row in retried {
            if let Some(cause) = attempt_retry_cause(retries, cell, row) {
                found.push(RetriedAttempt {
                    row,
                    cause,
                    final_outcome: final_outcome.clone(),
                });
            }
        }
    }
    found
}

/// Every SKID-RETRY in `histories`; see [`retried_attempts`].
fn skid_retries(
    retries: Retries,
    cells: &[SelectedCell],
    histories: &[Vec<CellResult>],
) -> Vec<SkidRetry> {
    retried_attempts(retries, cells, histories)
        .into_iter()
        .filter_map(|retried| match retried.cause {
            RetryCause::SkidOvershoot { reports } => Some(SkidRetry {
                test: retried.row.test.clone(),
                mode: retried.row.mode.clone(),
                backend: retried.row.backend.clone(),
                attempt: retried.row.attempt,
                reports,
                final_outcome: retried.final_outcome,
            }),
            _ => None,
        })
        .collect()
}

/// The summary.json entry for a PRODUCT-RETRY or HOST-INPUT-RETRY: the
/// retried attempt, what it observed and where its first divergence was, and
/// the outcome the cell's complete history selected. A final PASS here is a
/// failure the retry turned into a pass.
fn retried_attempt_json(retried: &RetriedAttempt<'_>) -> serde_json::Value {
    let row = retried.row;
    serde_json::json!({
        "test": row.test,
        "mode": row.mode,
        "backend": row.backend,
        "attempt": row.attempt,
        "outcome": row.outcome,
        "result": row.result,
        "first_divergent_scheduler_turn": row.first_divergent_scheduler_turn,
        "first_divergent_record": row.first_divergent_record,
        "first_divergent_syscall": row.first_divergent_syscall,
        "final_outcome": retried.final_outcome,
    })
}

/// The line printed for each PRODUCT-RETRY or HOST-INPUT-RETRY.
fn retried_attempt_line(label: &str, retried: &RetriedAttempt<'_>) -> String {
    let row = retried.row;
    let coords = [
        ("turn", row.first_divergent_scheduler_turn),
        ("rec", row.first_divergent_record),
        ("sys", row.first_divergent_syscall),
    ]
    .iter()
    .filter_map(|(key, value)| value.map(|value| format!(" {key}={value}")))
    .collect::<String>();
    let observed = row
        .result
        .and_then(|result| serde_json::to_value(result).ok())
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unrecorded".into());
    format!(
        "test-harness: {label} {} ({}/{}): attempt {} was {} ({observed}{coords}) and was retried; final outcome {}",
        row.test,
        row.mode,
        row.backend.as_deref().unwrap_or("native"),
        row.attempt,
        row.outcome,
        retried.final_outcome
    )
}

/// The line printed for a typed skid attempt that is not followed by a retry.
///
/// `history` is the cell's history ending with that attempt, and `imported`
/// says whether its rows were imported (`E2E_IMPORT_RESULTS`). At the shared
/// cap the line is `SKID-RETRY LIMIT`; below it the line is
/// `SKID-RETRY NOT MADE`, which says why. An imported skid attempt continues
/// to its producer's next attempt as an executed one would be retried
/// ([`imported_results`]), so it is the last one only when its producer
/// recorded no later attempt of the cell; an executed one is retried unless
/// its row could not be published. Either way it names the outcome
/// [`cell_result_after_retries`] selects from the whole history, which is not
/// always ERROR: a product failure earlier in the history outranks a final
/// infrastructure error.
fn unretried_skid_line(
    test: &str,
    mode: &str,
    backend: Option<&str>,
    attempt: u64,
    reports: u64,
    history: &[CellResult],
    imported: bool,
) -> String {
    let selected = match cell_result_after_retries(history) {
        Ok(result) => format!("{} (attempt {})", result.outcome, result.attempt),
        Err(error) => format!("ERROR (the history is inconsistent: {error})"),
    };
    let backend = backend.unwrap_or("native");
    if attempt >= MAX_ATTEMPTS_PER_CELL {
        format!(
            "SKID-RETRY LIMIT {test} ({mode}/{backend}): attempt {attempt} of at most {MAX_ATTEMPTS_PER_CELL} is a typed HERMIT_SKID_OVERSHOOT infrastructure error ({reports} report(s)); the attempt cap is reached, no further retry is made, and the cell's selected outcome is {selected}"
        )
    } else if imported {
        format!(
            "SKID-RETRY NOT MADE {test} ({mode}/{backend}): attempt {attempt} of at most {MAX_ATTEMPTS_PER_CELL} is a typed HERMIT_SKID_OVERSHOOT infrastructure error ({reports} report(s)), but it is an imported attempt and its producer recorded no later attempt of the cell; the cell's selected outcome is {selected}"
        )
    } else {
        format!(
            "SKID-RETRY NOT MADE {test} ({mode}/{backend}): attempt {attempt} of at most {MAX_ATTEMPTS_PER_CELL} is a typed HERMIT_SKID_OVERSHOOT infrastructure error ({reports} report(s)), but this run has no later attempt of the cell; the cell's selected outcome is {selected}"
        )
    }
}

/// The selection `run` applies for `args`: its filters, over the required
/// population unless manual cells are included.
fn run_selection(args: &Args) -> Selection {
    let mut selection = args.selection.clone();
    if selection.population.is_none() {
        selection.population = Some(if selection.include_manual {
            Population::Enabled
        } else {
            Population::Required
        });
    }
    selection
}

/// The cells `run` executes for `args`: its filters, over the required
/// population unless manual cells are included.
fn run_cells(manifests: &ManifestSet, args: &Args) -> Result<Vec<SelectedCell>, String> {
    manifests.select(&run_selection(args))
}

fn run(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let mut cells = run_cells(manifests, args).unwrap_or_else(|e| fail(e));
    if !args.selection.exclude_backends.is_empty() {
        let unfiltered = manifests
            .select(&Selection {
                exclude_backends: Vec::new(),
                ..run_selection(args)
            })
            .unwrap_or_else(|e| fail(e))
            .len();
        eprintln!(
            "Omitted by --exclude-backend {}: {} of {unfiltered} selected cells (not run, no result row)",
            args.selection.exclude_backends.join(","),
            unfiltered - cells.len()
        );
    }
    if cells.is_empty() && !args.allow_empty {
        fail("filters selected no cells");
    }
    if let Err(error) = require_declared_diagnostics(
        &cells,
        args.diagnostic_results,
        std::env::var_os("DAGRUN_TEST_COUNTS_PATH").is_some(),
    ) {
        fail(error);
    }
    let capacity = scheduled_worker_capacity(args);
    let import_root = std::env::var_os(IMPORT_RESULTS_ENV).map(PathBuf::from);
    let planned_verify = planned_verify(&cells);
    // An imported run measures the same scope from the verify logs its
    // producer retained, which the ingest restored below the import root
    // (`parity::ImportedLogs`).
    let parity_scope = parity_scope(manifests, &planned_verify);
    let context = if import_root.is_some() {
        RunContext::for_import(root.to_path_buf(), args.source_sha.as_deref())
    } else {
        RunContext::from_env(
            root.to_path_buf(),
            args.prebuilt,
            args.source_sha.as_deref(),
        )
    }
    .unwrap_or_else(|e| fail(e))
    .with_scheduled_worker_capacity(capacity)
    .with_parity_retained(parity::retention_closure(
        &parity_scope.cells,
        &planned_verify,
    ));
    for (capability, verdict) in &context.host_capabilities {
        eprintln!(
            "Host capability {}: {} — {}",
            capability.value(),
            if verdict.present { "PRESENT" } else { "ABSENT" },
            verdict.evidence
        );
    }
    // One run per test, at the deepest level this host supports; a verify cell
    // that caps itself runs relaxed everywhere. The level is the host's, so it
    // is set only here, where the cells run, and never in a plan. An import
    // runs nothing: its rows ran at their producer's level, and the host checks
    // it applies to them are the producer's, as before.
    if import_root.is_none() {
        let host_level = host_strictness(
            &context.host_capabilities,
            context.exact_branch_counter.as_ref(),
            std::env::var(STRICTNESS_LEVEL_ENV).ok().as_deref(),
        )
        .unwrap_or_else(|e| fail(e));
        eprintln!(
            "Strictness level: {}{}",
            host_level.level.as_str().to_uppercase(),
            host_level
                .reason
                .as_deref()
                .map(|reason| format!(
                    " — {reason}; verify cells on ptrace and e9patch add {}",
                    RELAXED_LEVEL_ARGS.join(" ")
                ))
                .unwrap_or_default()
        );
        for cell in &mut cells {
            cell.host_level = host_level.clone();
        }
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
    mark_parity_running(&parity_scope.cells, &context, &results_path);
    let imported = import_root.as_deref().map(|import_root| {
        let earns_retry = |cell: &SelectedCell, row: &CellResult| {
            attempt_earns_retry(args.retries, cell, row)
        };
        let host_inapplicable = |cell: &SelectedCell| {
            cell_host_inapplicable_reason(
                cell,
                &context.host_capabilities,
                context.exact_branch_counter.as_ref(),
            )
            .map(|(_, reason)| reason)
        };
        let policy = imported_results::ImportPolicy {
            earns_retry: &earns_retry,
            host_inapplicable: &host_inapplicable,
        };
        let imported =
            imported_results::load(import_root, &cells, &context, &results_path, &policy)
                .unwrap_or_else(|error| fail(format!("{IMPORT_RESULTS_ENV}: {error}")));
        eprintln!(
            "test-harness: importing {} selected cell(s) from {} (rows of {} run(s)); {} have no result; {} producer retr(ies) this run would not have made were dropped",
            cells.len(),
            import_root.display(),
            imported.source_run_ids.len(),
            imported.missing,
            imported.dropped_retries
        );
        imported
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
            if let Some(imported) = &imported {
                let rows = &imported.cells[index].rows;
                for (position, row) in rows.iter().enumerate() {
                    if !emit(row.clone(), position + 1 < rows.len()) {
                        break;
                    }
                }
                return;
            }
            if let Some((_, reason)) = cell_host_inapplicable_reason(
                cell,
                &context.host_capabilities,
                context.exact_branch_counter.as_ref(),
            ) {
                let _ = emit(host_inapplicable_result(&context, cell, reason), false);
                return;
            }

            run_with_retry(
                context.attempt,
                |attempt| {
                    let attempt_context = context.with_attempt(attempt);
                    match run_cell(&attempt_context, cell) {
                        Ok(result) => result,
                        Err(error) => error.into_result(&attempt_context, cell),
                    }
                },
                |result| attempt_earns_retry(args.retries, cell, result),
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
                    result.reason_for_display()
                );
            }
            // A FAILURE MUST SAY ENOUGH TO BE CLASSIFIED, NOT JUST COUNTED.
            let located = if result.outcome == "PASS" {
                String::new()
            } else if result.outcome == "HOST-INAPPLICABLE" {
                format!(" {}", result.reason_for_display())
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
                if result.outcome != "PASS" {
                    suffix.push_str(&format!(" {}", result.reason_for_display()));
                }
                suffix.push_str(&format!("\n    evidence: {}", result.artifact_dir));
                suffix
            };
            let effective_will_retry = published && will_retry;
            let retry_cause = attempt_retry_cause(args.retries, &cells[index], &result);
            let retry_note = match (effective_will_retry, retry_cause) {
                (true, Some(RetryCause::SkidOvershoot { reports })) => format!(
                    " [SKID-RETRY: attempt {} of at most {} is a typed HERMIT_SKID_OVERSHOOT infrastructure error ({reports} report(s)) and nothing else; retrying this cell only]",
                    result.attempt, MAX_ATTEMPTS_PER_CELL
                ),
                (true, Some(RetryCause::HostInputChanged)) => format!(
                    " [HOST-INPUT-RETRY: attempt {} of at most {} diverged because a host file changed during a run, which is not a product failure; retrying this cell once, and a second divergence stays a failure]",
                    result.attempt, MAX_ATTEMPTS_PER_CELL
                ),
                (true, _) => format!(
                    " [attempt {} of at most {}; retrying this cell only]",
                    result.attempt, MAX_ATTEMPTS_PER_CELL
                ),
                (false, _) => String::new(),
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
            let unretried_skid = match (effective_will_retry, retry_cause) {
                (false, Some(RetryCause::SkidOvershoot { reports })) => Some((
                    reports,
                    result.test.clone(),
                    result.mode.clone(),
                    result.backend.clone(),
                    result.attempt,
                )),
                _ => None,
            };

            attempt_results[index].push(result);
            if let Some((reports, test, mode, backend, attempt)) = unretried_skid {
                // A skid attempt that is not retried: say why, and which outcome
                // the cell's whole history selected, where the cell finishes.
                println!(
                    "{}",
                    unretried_skid_line(
                        &test,
                        &mode,
                        backend.as_deref(),
                        attempt,
                        reports,
                        &attempt_results[index],
                        imported.is_some(),
                    )
                );
            }
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
                if let Some(reason) =
                    excused_diagnostic(args.diagnostic_results, &result, &attempt_results[index])
                {
                    // Reported and counted, never silent; it does not fail the run.
                    println!(
                        "DIAGNOSTIC {} ({}/{}): this product failure does not fail the run: {reason}",
                        result.test,
                        result.mode,
                        result.backend.as_deref().unwrap_or("native"),
                    );
                } else {
                    failed |= matches!(result.outcome.as_str(), "FAIL" | "ERROR");
                }
                indexed_results.push((index, result));
            }
            published
        },
    );
    // Why the run fails beyond its failed cells. Each reason fails the exit
    // status AND is reported to Tpx, so the two cannot disagree.
    let mut run_failures = Vec::new();
    let returned = indexed_results
        .iter()
        .map(|(index, _)| *index)
        .collect::<BTreeSet<_>>();
    let missing = (0..expected)
        .filter(|index| !returned.contains(index))
        .map(|index| tpx_test_name(&cells[index].id))
        .collect::<Vec<_>>();
    if indexed_results.len() != expected {
        let reason = format!(
            "only {} of {expected} selected cells returned a result",
            indexed_results.len()
        );
        eprintln!("test-harness: {reason}");
        run_failures.push(reason);
    }
    indexed_results.sort_by_key(|(index, _)| *index);
    let diagnostic_cells = indexed_results
        .iter()
        .filter_map(|(index, result)| {
            let reason =
                excused_diagnostic(args.diagnostic_results, result, &attempt_results[*index])?;
            Some(serde_json::json!({
                "test": result.test,
                "mode": result.mode,
                "backend": result.backend,
                "diagnostic_reason": reason,
                "failure_reason": result.reason,
            }))
        })
        .collect::<Vec<_>>();
    // Parallel to `results`: whether each final cell is an excused diagnostic.
    let excused = indexed_results
        .iter()
        .map(|(index, result)| {
            excused_diagnostic(args.diagnostic_results, result, &attempt_results[*index]).is_some()
        })
        .collect::<Vec<_>>();
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();
    // Every SKID-RETRY stays visible: a count on the summary line, one line
    // per retry, and both in summary.json. Each retried attempt's row is also
    // kept in results.jsonl.
    let skid_retried = skid_retries(args.retries, &cells, &attempt_results);
    if expected > 0 {
        println!(
            "test-harness: completed {} cell(s) with up to {} concurrent worker(s); SKID-RETRY count {}",
            results.len(),
            capacity.workers_for(expected),
            skid_retried.len()
        );
    }
    for retry in &skid_retried {
        println!(
            "test-harness: SKID-RETRY {} ({}/{}): attempt {} recorded {} HERMIT_SKID_OVERSHOOT report(s) and was retried; final outcome {}",
            retry.test,
            retry.mode,
            retry.backend.as_deref().unwrap_or("native"),
            retry.attempt,
            retry.reports,
            retry.final_outcome
        );
    }
    // The other retries are counted the same way: a PRODUCT-RETRY follows a
    // measured product failure, including a verify divergence without an
    // overshoot marker, and a final PASS selected after one is a failure the
    // retry turned into a pass (https://github.com/rrnewton/hermit/issues/1845).
    let retried = retried_attempts(args.retries, &cells, &attempt_results);
    let product_retried = retried
        .iter()
        .filter(|retried| retried.cause == RetryCause::ProductFailure)
        .collect::<Vec<_>>();
    let host_input_retried = retried
        .iter()
        .filter(|retried| retried.cause == RetryCause::HostInputChanged)
        .collect::<Vec<_>>();
    if expected > 0 {
        println!(
            "test-harness: PRODUCT-RETRY count {} ({} final PASS); HOST-INPUT-RETRY count {}",
            product_retried.len(),
            product_retried
                .iter()
                .filter(|retried| retried.final_outcome == "PASS")
                .count(),
            host_input_retried.len()
        );
    }
    for retried in &product_retried {
        println!("{}", retried_attempt_line("PRODUCT-RETRY", retried));
    }
    for retried in &host_input_retried {
        println!("{}", retried_attempt_line("HOST-INPUT-RETRY", retried));
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
        let written = if args.diagnostic_results {
            structured_test_results(&attempt_results).and_then(|report| {
                report
                    .write_diagnostic_typed(&path)
                    .map_err(|error| error.to_string())
            })
        } else {
            terminal_test_results(&attempt_results).and_then(|report| report.write_current(&path))
        };
        if let Err(error) = written {
            eprintln!("test-harness: {error}");
            run_failures.push(format!("cannot write DAGRUN_TEST_COUNTS_PATH: {error}"));
        }
    }
    if let Some(reason) = vacuity_refusal(expected, host_inapplicable) {
        eprintln!("test-harness: {reason}");
        run_failures.push(reason);
    }
    failed |= !run_failures.is_empty();
    write_junit(&junit, &results).unwrap();
    let mut summary = serde_json::json!({
        "schema": 1,
        "cells": results.len(),
        "passed": results.iter().filter(|result| result.outcome == "PASS").count(),
        "failed": results.iter().filter(|result| result.outcome == "FAIL").count(),
        "errors": results.iter().filter(|result| result.outcome == "ERROR").count(),
        // A subset of `failed`: product failures of diagnostic cells, which do
        // not fail the run. Counted and named separately so they stay visible.
        "diagnostic_failures": diagnostic_cells.len(),
        "diagnostic_failure_cells": diagnostic_cells,
        // Retries of a verify attempt whose only failure was Hermit's typed
        // precise-timer overshoot (https://github.com/rrnewton/hermit/issues/1845).
        "skid_retries": skid_retried.len(),
        "skid_retry_cells": skid_retried
            .iter()
            .map(|retry| serde_json::json!({
                "test": retry.test,
                "mode": retry.mode,
                "backend": retry.backend,
                "attempt": retry.attempt,
                "overshoot_reports": retry.reports,
                "final_outcome": retry.final_outcome,
            }))
            .collect::<Vec<_>>(),
        // Retries of a measured product failure, and of a divergence a host
        // file changing explains; see the PRODUCT-RETRY lines above.
        "product_retries": product_retried.len(),
        "product_retry_cells": product_retried
            .iter()
            .map(|retried| retried_attempt_json(retried))
            .collect::<Vec<_>>(),
        "host_input_retries": host_input_retried.len(),
        "host_input_retry_cells": host_input_retried
            .iter()
            .map(|retried| retried_attempt_json(retried))
            .collect::<Vec<_>>(),
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
    if let (Some(import_root), Some(imported)) = (&import_root, &imported) {
        summary["imported"] = serde_json::json!({
            // Absolute, so `parity compare` finds the restored logs from any
            // directory (`recorded_import_root`).
            "root": std::path::absolute(import_root).unwrap_or_else(|_| import_root.clone()),
            "source_run_ids": imported.source_run_ids,
            "missing_cells": imported.missing,
            "dropped_retries": imported.dropped_retries,
        });
    }
    fs::write(
        results_path.parent().unwrap().join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    if let Some(path) = &args.tpx_json {
        if let Err(error) = write_tpx_json(path, &results, &excused, &missing, &run_failures) {
            eprintln!("test-harness: cannot write {}: {error}", path.display());
            failed = true;
        }
    }
    let exit = if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    };
    // Every determinism output above is final. The parity post-pass only
    // reads it, and nothing it does reaches `exit`.
    report_parity(
        &parity_scope,
        &context,
        &results_path,
        capacity,
        &attempt_results,
        import_root.as_deref(),
    );
    exit
}

/// The refusal of a run in which every selected cell was host-inapplicable:
/// it executed nothing, so it is not a pass.
fn vacuity_refusal(expected: usize, host_inapplicable: usize) -> Option<String> {
    (expected > 0 && host_inapplicable == expected).then(|| {
        format!(
            "every one of the {expected} selected cell(s) was host-inapplicable; \
             a run that executed no cell is not a pass"
        )
    })
}

/// A cell's Tpx test name: `test/mode@backend`, `native` without a backend.
fn tpx_test_name(id: &CellId) -> String {
    format!(
        "{}/{}@{}",
        id.test,
        id.mode,
        id.backend.as_deref().unwrap_or("native")
    )
}

/// The Tpx test name of the record that carries run-level failures.
const TPX_RUN_VERDICT_TEST: &str = "test-harness/run-verdict";

/// Write one Tpx HPHP-JSON `test_done` record per final cell, then `all_done`,
/// so a Buck `type = "json"` test reports each cell as its own test case.
///
/// The status follows the run's own verdict: PASS is `passed`; a
/// host-inapplicable cell ran nothing and an excused diagnostic failure does
/// not fail the run, so both are `skipped`; any other outcome is `failed`.
/// `details` carries the row's verdict fields, so a skip still says why.
///
/// A run can fail without a failed cell, so the failure is reported too: each
/// selected cell that returned no result is a `failed` record named by its
/// identity, and the run-level `run_failures` (the all-host-inapplicable
/// refusal, an unwritable test-count file, missing results) are one `failed`
/// [`TPX_RUN_VERDICT_TEST`] record. Otherwise Tpx would read a run whose exit
/// status failed as passed or skipped.
fn write_tpx_json(
    path: &Path,
    results: &[CellResult],
    excused: &[bool],
    missing: &[String],
    run_failures: &[String],
) -> std::io::Result<()> {
    let mut lines = String::new();
    let mut push = |test: &str, status: &str, details: serde_json::Value| {
        let record = serde_json::json!({
            "op": "test_done",
            "test": test,
            "status": status,
            "details": details.to_string(),
        });
        lines.push_str(&record.to_string());
        lines.push('\n');
    };
    for (result, excused) in results.iter().zip(excused) {
        let status = match result.outcome.as_str() {
            "PASS" => "passed",
            "HOST-INAPPLICABLE" => "skipped",
            _ if *excused => "skipped",
            _ => "failed",
        };
        let details = serde_json::json!({
            "outcome": result.outcome,
            "result": result.result,
            "failure_class": result.failure_class,
            "error_kind": result.error_kind,
            "reason": result.reason,
            "attempt": result.attempt,
            "duration_ms": result.duration_ms,
            "cpu_usage_usec": result.cpu_usage_usec,
            "artifact_dir": result.artifact_dir,
        });
        let test = tpx_test_name(&CellId {
            test: result.test.clone(),
            mode: result.mode.clone(),
            backend: result.backend.clone(),
        });
        push(&test, status, details);
    }
    for test in missing {
        push(
            test,
            "failed",
            serde_json::json!({
                "outcome": null,
                "reason": "the selected cell returned no result",
            }),
        );
    }
    if !run_failures.is_empty() {
        push(
            TPX_RUN_VERDICT_TEST,
            "failed",
            serde_json::json!({ "run_failures": run_failures }),
        );
    }
    lines.push_str(&serde_json::json!({ "op": "all_done" }).to_string());
    lines.push('\n');
    fs::write(path, lines)
}

/// The `(test, backend)` verify cells this process plans.
fn planned_verify(cells: &[SelectedCell]) -> BTreeSet<(String, String)> {
    cells
        .iter()
        .filter(|cell| cell.id.mode == parity::PARITY_MODE)
        .filter_map(|cell| {
            cell.id
                .backend
                .clone()
                .map(|backend| (cell.id.test.clone(), backend))
        })
        .collect()
}

/// The parity cells this run reports on: the parity population and
/// `E2E_PARITY_SELECT`, resolved by [`parity::resolve_scope`], the same rule
/// the pressure test uses: the cells with a verify side planned here, with
/// why each candidate full validation does not run has no log. An explicit
/// cell with neither side planned is dropped with a warning.
/// `E2E_PARITY_POST_PASS=0` turns the post-pass off. A matrix that cannot be
/// derived only warns, because the parity report must never stop the
/// determinism cells. An invalid `E2E_PARITY_SELECT` or `E2E_PARITY_POST_PASS`
/// is a usage error, refused before any cell runs or any output is created.
fn parity_scope(
    manifests: &ManifestSet,
    planned: &BTreeSet<(String, String)>,
) -> parity::ResolvedScope {
    match std::env::var(parity::PARITY_POST_PASS_ENV) {
        Err(std::env::VarError::NotPresent) => {}
        Ok(value) if value == "1" => {}
        Ok(value) if value == "0" => return parity::ResolvedScope::default(),
        Ok(value) => fail(format!(
            "{} must be 0 or 1, got {value:?}",
            parity::PARITY_POST_PASS_ENV
        )),
        Err(std::env::VarError::NotUnicode(_)) => {
            fail(format!("{} must be 0 or 1", parity::PARITY_POST_PASS_ENV))
        }
    }
    let explicit = std::env::var(parity::PARITY_SELECT_ENV).ok();
    match parity::resolve_scope(manifests, explicit.as_deref(), planned) {
        Ok(scope) => {
            for warning in &scope.warnings {
                eprintln!("test-harness: {warning}");
            }
            scope
        }
        Err(error) => fail(format!("{}: {error}", parity::PARITY_SELECT_ENV)),
    }
}

/// Before any cell runs, remove an earlier run's parity outputs and, with a
/// cell in scope, write `parity.status.json` in the `running` state naming
/// every cell in scope. A process killed before its post-pass finishes then
/// still says which parity cells it owed, so the ledger export
/// ([`parity::node_ledger_sources`]) reports each as `record-missing` instead
/// of dropping it. Like the post-pass, a failure here is reported and changes
/// nothing else.
fn mark_parity_running(scope: &BTreeSet<ParityCellId>, context: &RunContext, results_path: &Path) {
    let Some(artifacts) = results_path.parent() else {
        return;
    };
    let config = parity::PostPassConfig::new(
        artifacts,
        &context.hermit_bin,
        &context.run_id,
        &context.source_sha,
    );
    let marked = if scope.is_empty() {
        parity::clear_outputs(&config)
    } else {
        parity::mark_running(&config, scope)
    };
    if let Err(error) = marked {
        eprintln!(
            "test-harness: parity status not marked running (exit status unaffected): {error}"
        );
    }
}

/// Run the parity post-pass over this process's rows once `results.jsonl`,
/// JUnit and `summary.json` are final. It writes only `parity.jsonl`,
/// `parity.status.json` and `parity/` beside them, inside the enclosing
/// dagrun step's wall bound, and returns nothing: an error, or even a panic,
/// is reported and the exit status stays what determinism made it. With no
/// cell in scope it only removes an earlier run's parity outputs. For an
/// imported run (`import_root`) the logs are the ones the ingest restored
/// below that root ([`parity::ImportedLogs`]).
fn report_parity(
    resolved: &parity::ResolvedScope,
    context: &RunContext,
    results_path: &Path,
    capacity: ScheduledWorkerCapacity,
    attempt_results: &[Vec<CellResult>],
    import_root: Option<&Path>,
) {
    let Some(artifacts) = results_path.parent() else {
        eprintln!("test-harness: parity post-pass skipped: results path has no directory");
        return;
    };
    let scope = &resolved.cells;
    let mut config = parity::PostPassConfig::new(
        artifacts,
        &context.hermit_bin,
        &context.run_id,
        &context.source_sha,
    )
    .naming_roots(&context.result_root, &context.root);
    config.jobs = capacity.workers_for(scope.len());
    config.outer_deadline = parity::dagrun_step_deadline();
    config.imported_logs = import_root.map(parity::ImportedLogs::load);
    config.candidate_not_run = resolved.candidate_not_run.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        if std::env::var_os("HERMIT_PARITY_POST_PASS_PANIC").is_some() {
            panic!("planted harness post-pass panic");
        }
        if scope.is_empty() {
            return parity::clear_outputs(&config).map(|()| None);
        }
        let rows = attempt_results.concat();
        parity::post_pass(&config, scope, &rows).map(Some)
    }));
    match outcome {
        Ok(Ok(Some(report))) => print_best_effort(
            std::io::stdout(),
            &format!("test-harness: {}", report.summary_line()),
        ),
        Ok(Ok(None)) => {}
        Ok(Err(error)) => {
            eprintln!("test-harness: parity post-pass failed (exit status unaffected): {error}")
        }
        Err(_) => eprintln!("test-harness: parity post-pass panicked (exit status unaffected)"),
    }
}

/// Print one line that must not decide the exit status: a closed or full
/// stdout is ignored rather than panicking as `println!` does.
fn print_best_effort(mut out: impl Write, line: &str) {
    let _ = writeln!(out, "{line}");
}

/// Below the run's artifacts, where `parity compare` writes everything.
const PARITY_COMPARE_DIR: &str = "parity-compare";

struct ParityCompareRequest {
    artifacts: PathBuf,
    cells: Vec<String>,
    output: Option<PathBuf>,
    jobs: usize,
}

#[derive(Debug, PartialEq)]
struct ParityExportRequest {
    e2e_root: PathBuf,
    expected_scope: Option<PathBuf>,
}

enum ParityRequest {
    Compare(ParityCompareRequest),
    Export(ParityExportRequest),
}

fn parse_parity(values: Vec<String>) -> ParityRequest {
    if values.iter().any(|value| is_help_flag(value)) {
        println!("{PARITY_HELP}");
        std::process::exit(0);
    }
    let mut values = values.into_iter();
    match values.next().as_deref() {
        Some("compare") => ParityRequest::Compare(parse_parity_compare(values)),
        Some("export") => ParityRequest::Export(parse_parity_export(values)),
        Some(other) => fail(format!(
            "unknown parity command {other:?}; expected `parity compare` or `parity export`"
        )),
        None => fail("parity requires a command; try `test-harness parity --help`"),
    }
}

fn parse_parity_export(mut values: impl Iterator<Item = String>) -> ParityExportRequest {
    let mut e2e_root = None;
    let mut expected_scope = None;
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--e2e-root" => set_once(&mut e2e_root, &mut values, "--e2e-root"),
            "--expected-scope" => set_once(&mut expected_scope, &mut values, "--expected-scope"),
            other => fail(format!(
                "unknown option {other} for parity export; try `test-harness parity --help`"
            )),
        }
    }
    ParityExportRequest {
        e2e_root: PathBuf::from(e2e_root.unwrap_or_else(|| {
            fail(
                "parity export requires --e2e-root <DIR>, the run's e2e result root \
                     holding <lane>/<node>/parity.status.json",
            )
        })),
        expected_scope: expected_scope.map(PathBuf::from),
    }
}

/// `test-harness parity export`: the ledger source rows of a finished run's
/// post-passes, one JSON object per line, for `series.py append-parity`.
fn parity_export(request: &ParityExportRequest) -> ExitCode {
    let expected = request.expected_scope.as_ref().map(|path| {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", path.display())));
        parity::parse_expected_scope(&text)
            .unwrap_or_else(|error| fail(format!("{}: {error}", path.display())))
    });
    let rows =
        parity::ledger_sources(&request.e2e_root, expected.as_ref()).unwrap_or_else(|error| {
            fail(format!(
                "parity export printed no rows: {error}. Inspect the named parity file; \
                 `test-harness parity compare --artifacts <that node> --cell <TEST@BACKEND>` \
                 re-measures a cell from the run's retained logs"
            ))
        });
    let mut out = std::io::stdout().lock();
    for row in &rows {
        let line = serde_json::to_string(row).expect("a validated ledger source row serializes");
        if let Err(error) = writeln!(out, "{line}") {
            fail(format!("cannot write parity export rows: {error}"));
        }
    }
    eprintln!(
        "test-harness: parity export: {} source row(s) from {}",
        rows.len(),
        request.e2e_root.display()
    );
    ExitCode::SUCCESS
}

fn parse_parity_compare(mut values: impl Iterator<Item = String>) -> ParityCompareRequest {
    let mut artifacts = None;
    let mut output = None;
    let mut jobs = None;
    let mut cells = Vec::new();
    while let Some(flag) = values.next() {
        match flag.as_str() {
            "--artifacts" => set_once(&mut artifacts, &mut values, "--artifacts"),
            "--output" => set_once(&mut output, &mut values, "--output"),
            "--jobs" => set_once(&mut jobs, &mut values, "--jobs"),
            "--cell" => cells.push(required_value(&mut values, "--cell")),
            other => fail(format!("unknown option {other} for parity compare")),
        }
    }
    let artifacts = artifacts.unwrap_or_else(|| fail("parity compare requires --artifacts <DIR>"));
    if cells.is_empty() {
        fail("parity compare requires at least one --cell <TEST@BACKEND>");
    }
    let jobs = jobs.map_or(1, |jobs| match jobs.parse::<usize>() {
        Ok(jobs) if jobs > 0 => jobs,
        _ => fail(format!("--jobs must be a positive integer, got {jobs:?}")),
    });
    ParityCompareRequest {
        artifacts: PathBuf::from(artifacts),
        cells,
        output: output.map(PathBuf::from),
        jobs,
    }
}

/// `test-harness parity compare`: the same post-pass over a finished run's
/// `results.jsonl` and retained logs, for cells named on the command line.
fn parity_compare(
    root: &Path,
    manifests: &ManifestSet,
    request: &ParityCompareRequest,
) -> ExitCode {
    let matrix = parity::ParityMatrix::derive(manifests).unwrap_or_else(|error| fail(error));
    let scope = request
        .cells
        .iter()
        .map(|cell| parity::parse_parity_cell(cell, &matrix).unwrap_or_else(|error| fail(error)))
        .collect::<BTreeSet<_>>();
    let results = request.artifacts.join("results.jsonl");
    let text = fs::read_to_string(&results)
        .unwrap_or_else(|error| fail(format!("cannot read {}: {error}", results.display())));
    let rows = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<CellResult>(line).unwrap_or_else(|error| {
                fail(format!("{}:{}: {error}", results.display(), index + 1))
            })
        })
        .collect::<Vec<_>>();
    let only = |values: BTreeSet<&str>, what: &str| -> String {
        match values.into_iter().collect::<Vec<_>>().as_slice() {
            [value] => (*value).to_string(),
            [] => fail(format!("{} has no result rows", results.display())),
            many => fail(format!(
                "{} mixes {} {what} values ({}); compare one run at a time",
                results.display(),
                many.len(),
                many.join(", ")
            )),
        }
    };
    let run_id = only(
        rows.iter().map(|row| row.run_id.as_str()).collect(),
        "run_id",
    );
    let hermit_sha = only(
        rows.iter().map(|row| row.hermit_sha.as_str()).collect(),
        "hermit_sha",
    );
    let mut config = parity_compare_config(root, request, &run_id, &hermit_sha)
        .unwrap_or_else(|error| fail(error));
    config.candidate_not_run = matrix.candidates_not_run(&scope);
    let report = parity::post_pass(&config, &scope, &rows).unwrap_or_else(|error| fail(error));
    for record in &report.records {
        println!(
            "{}",
            serde_json::to_string(record).expect("a validated parity record serializes")
        );
    }
    eprintln!("test-harness: {}", report.summary_line());
    ExitCode::SUCCESS
}

/// The post-pass configuration `parity compare` measures the finished run in
/// `request.artifacts` with: its outputs below that run's
/// `PARITY_COMPARE_DIR`, and an imported run's logs read from where its
/// ingest restored them (`recorded_import_root`).
fn parity_compare_config(
    root: &Path,
    request: &ParityCompareRequest,
    run_id: &str,
    hermit_sha: &str,
) -> Result<parity::PostPassConfig, String> {
    let import_root = recorded_import_root(&request.artifacts)?;
    // An imported run's logs are compared with the Hermit its Buck cells ran, as its own
    // post-pass compares them (`RunContext::for_import`).
    let hermit_bin = if import_root.is_some() {
        hermit_manifest_plan::runner::resolve_import_hermit_bin(
            root,
            std::env::var_os("HERMIT_BIN"),
        )
    } else {
        hermit_manifest_plan::runner::resolve_hermit_bin(root, std::env::var_os("HERMIT_BIN"))
    };
    let mut config =
        parity::PostPassConfig::new(&request.artifacts, &hermit_bin, run_id, hermit_sha)
            .writing_below(&request.artifacts.join(PARITY_COMPARE_DIR));
    if let Some(output) = &request.output {
        config.output = output.clone();
    }
    config.jobs = request.jobs;
    config.outer_deadline = parity::dagrun_step_deadline();
    config.imported_logs = import_root.map(|root| parity::ImportedLogs::load(&root));
    Ok(config)
}

/// The import root the finished run in `artifacts` recorded in its
/// summary.json, or None when it imported nothing. Its rows record verify-log
/// directories on the machines that ran the cells, so `parity compare` must
/// read an imported run's logs from where the ingest restored them, as the
/// run's own post-pass does (`report_parity`); without its summary, whether
/// the run imported is unknown, so the comparison is refused.
fn recorded_import_root(artifacts: &Path) -> Result<Option<PathBuf>, String> {
    let path = artifacts.join("summary.json");
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read {}, so whether the run imported its rows is unknown: {error}",
            path.display()
        )
    })?;
    let summary: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let Some(summary) = summary.as_object() else {
        return Err(format!(
            "{} is not a JSON object, so whether the run imported its rows is unknown",
            path.display()
        ));
    };
    let Some(imported) = summary.get("imported") else {
        return Ok(None);
    };
    match imported.get("root").and_then(serde_json::Value::as_str) {
        Some(root) if Path::new(root).is_absolute() => Ok(Some(PathBuf::from(root))),
        _ => Err(format!(
            "{} records an import with no absolute \"root\", so the verify logs it restored \
             cannot be found",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use hermit_manifest_plan::parity;
    use hermit_manifest_plan::runner::FailureClass;
    use hermit_manifest_plan::runner::ManifestSet;
    use hermit_manifest_plan::runner::ScheduledWorkerCapacity;

    use super::CellResult;
    use super::DEFAULT_BUILD_JOBS;
    use super::DEFAULT_VALIDATE_AUDIT_JOBS;
    use super::EXPECTED_PLAN_SCHEMA;
    use super::HostCapability;
    use super::HostCapabilityVerdict;
    use super::JsonValue;
    use super::PINNED_COMMAND_PREFIX;
    use super::PINNED_COMMAND_SEPARATOR;
    use super::PREBUILT_COMMAND_PREFIX;
    use super::SelectedCell;
    use super::TestResults;
    use super::YamlValue;
    use super::accumulate_cell_cpu_usage;
    use super::audit_privileged_unboxed_guard;
    use super::audit_run_dag_workflow_runner;
    use super::audit_validation_levels_policy;
    use super::build_worker_capacity;
    use super::cell_host_inapplicable_reason;
    use super::cell_result_is_retryable;
    use super::command_jobs;
    use super::command_runs_exactly;
    use super::command_timeout_seconds;
    use super::excused_diagnostic;
    use super::expected_plan_document;
    use super::for_each_parallel;
    use super::host_inapplicable_reason;
    use super::parse;
    use super::planned_verify;
    use super::print_best_effort;
    use super::required_plan_rows;
    use super::run_cells;
    use super::run_with_retry;
    use super::scheduled_worker_capacity;
    use super::shell_quote_one;
    use super::structured_test_results;
    use super::unique_plan_rows;
    use super::validate_args;
    use super::validation_audit_worker_capacity;

    /// Before <https://github.com/rrnewton/hermit/issues/3301>, `run
    /// --parity-reference ptrace` sent every non-ptrace verify cell through a
    /// second ptrace run and a `hermit log-diff` comparison that could turn the
    /// candidate's PASS into a FAIL. The same mixed selection now runs each
    /// verify cell once, on its own backend, even when the candidate's log
    /// differs from ptrace's ("diverged").
    #[test]
    fn production_mixed_selection_runs_each_verify_cell_on_its_own_backend() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::process::Command;
        use std::process::ExitCode;

        use hermit_manifest_plan::runner::ObservedResult;
        use serde_json::json;

        const CHILD: &str = "HERMIT_MIXED_ORDINARY_FIXTURE";
        const TEST: &str =
            "tests::production_mixed_selection_runs_each_verify_cell_on_its_own_backend";
        if let Some(fixture) = std::env::var_os(CHILD) {
            let fixture = PathBuf::from(fixture);
            let values = vec![
                "--category".into(),
                "parity".into(),
                "--ci-only".into(),
                "--prebuilt".into(),
                "--jobs".into(),
                "1".into(),
                "--results".into(),
                fixture.join("results.jsonl").display().to_string(),
                "--junit".into(),
                fixture.join("junit.xml").display().to_string(),
            ];
            let args = parse(values.into_iter());
            validate_args("run", &args);
            let manifests = ManifestSet::load(&fixture).unwrap();
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            assert_eq!(super::run(&root, &manifests, &args), ExitCode::SUCCESS);
            return;
        }

        for scenario in ["matched", "diverged"] {
            let fixture = std::env::temp_dir().join(format!(
                "hermit-harness-mixed-ordinary-{}-{:?}-{scenario}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir(&fixture).unwrap();
            let path = fixture.as_path();
            fs::write(path.join("scenario"), scenario).unwrap();
            let manifests = path.join("tests/e2e/manifests");
            fs::create_dir_all(&manifests).unwrap();
            fs::write(
                manifests.join("defaults.yaml"),
                "schema: 3\ntimeout_seconds: 10\ncpu_timeout_seconds: 5\n",
            )
            .unwrap();
            let disabled = json!({"ci": false, "backends_enabled": [], "backends_disabled": {
                "ptrace": "Not selected by this control", "dbt": "Not selected by this control",
                "kvm": "Not selected by this control", "sabre": "Not selected by this control",
                "liteinst": "Not selected by this control",
                "in-guest-trap": "Not selected by this control"
            }});
            fs::write(manifests.join("parity.yaml"), serde_json::to_vec(&json!({
                "schema": 3, "bucket": "parity", "test": [{
                    "id": "parity/mixed", "description": "Native mixed routing control",
                    "lane": "portable", "occasional": false, "direct": ["/bin/true"],
                    "observation": {"status": true, "stdout": true, "stderr": true},
                    "modes": {
                        "verify": {"ci": true, "backends_enabled": ["ptrace", "kvm"],
                            "backends_disabled": {"dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected", "in-guest-trap": "Not selected"}},
                        "naked": {"ci": false, "backends_enabled": [],
                            "backends_disabled": {"native": "Not selected by this CI control"}},
                        "chaos": disabled, "replay": disabled,
                        "custom": {"ci": true, "backends_enabled": ["kvm"],
                            "backends_disabled": {"ptrace": "Not selected", "dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected", "in-guest-trap": "Not selected"},
                            "assert": {"runs": 1}}
                    }
                }]
            })).unwrap()).unwrap();
            let output = json!({"exit_code": 0, "signal": null,
                "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "stdout_bytes": 0, "stderr_bytes": 0});
            let verification = json!({
                "verified": true, "bitwise_parity": true, "verdict": "matched",
                "no_result_reason": null, "infrastructure_error": null,
                "comparison": {"strictness": "canonical", "display_name": "BitwiseInfoV1",
                    "compare_logs": true, "compare_io_buffers": true, "log_scope": "info",
                    "record_envelope": "all_records_v1", "virtualize_time": true,
                    "strip_lines": false, "canonicalize_addresses": true, "full_trace": true,
                    "exact_remainder": true, "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                    "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                    "ignore_lines": false, "skip_commit": false, "skip_detlog": false},
                "compared_log_messages": {"left": 2, "right": 2},
                "compared_outputs": {"left": output, "right": output},
                "guest_exit_code": 0, "guest_signal": null,
                "first_divergent_scheduler_turn": null, "first_divergent_virtual_nanoseconds": null,
                "first_divergent_record": null, "first_divergent_syscall": null,
                "first_divergent_left_message": null, "first_divergent_right_message": null
            });
            hermit_manifest_plan::canonical_verdict::VerificationReport::from_current_json_slice(
                &serde_json::to_vec(&verification).unwrap(),
            )
            .unwrap()
            .require_canonical_match()
            .unwrap();
            fs::write(
                path.join("verification.json"),
                serde_json::to_vec(&verification).unwrap(),
            )
            .unwrap();
            let backend = path.join("native-backend-adapter");
            fs::write(
                &backend,
                r#"#!/usr/bin/python3
import json,pathlib,sys
root=pathlib.Path(__file__).parent
full=sys.argv[1:]
a=full
if '--help' in a:
 print('--verify-strict');sys.exit(0)
if 'run' in a:a=a[a.index('run'):]
if not a or a[0] not in ('run','log-diff'):sys.exit(0)
scenario=(root/'scenario').read_text()
def record(value):
 with (root/'invocations').open('a') as f:f.write(json.dumps(value)+'\n')
if a[0]=='log-diff':
 if len(a)==2:
  record({'kind':'normalize','argv':a});sys.stdout.buffer.write(pathlib.Path(a[1]).read_bytes());sys.exit(0)
 record({'kind':'compare','argv':a});sys.exit(3)
# `--backend` is a global option, so it precedes `run`.
backend=full[full.index('--backend')+1]
if '--verify-json' not in a:
 assert '--verify' not in a and '--verify-strict' not in a,a
 record({'kind':'custom','backend':backend,'argv':a});sys.exit(0)
report=pathlib.Path(a[a.index('--verify-json')+1])
logdir=pathlib.Path(a[a.index('--verify-log-dir')+1])
reference='parity-reference' in str(logdir)
record({'kind':'run','backend':backend,'reference':reference,'argv':a})
assert '--verify-strict' in a and '--verify' in a,a
logdir.mkdir(parents=True,exist_ok=True)
value='different' if scenario=='diverged' and backend=='kvm' else 'shared'
(logdir/'run1_log_fixture.log').write_text('INFO detcore: '+value+'\nINFO detcore: complete\n')
report.write_bytes((root/'verification.json').read_bytes())
"#,
            )
            .unwrap();
            fs::set_permissions(&backend, fs::Permissions::from_mode(0o755)).unwrap();
            let result = Command::new("timeout")
                .args(["--kill-after=2s", "30s"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env(CHILD, path)
                .env("HERMIT_BIN", &backend)
                .env("E2E_RESULT_ROOT", path.join("artifacts"))
                .env("E2E_BUILD_ROOT", path.join("build"))
                .env("E2E_RUN_ID", "mixed-ordinary-control")
                .env("E2E_MACHINE_SHORTNAME", "native-control")
                .env("E2E_KERNEL_VERSION", "native-control")
                .env("E2E_KEEP_VERIFY_LOGS", "1")
                // parity/mixed's ptrace verify cell is required, so its kvm
                // cell is in the parity population and the post-pass would
                // compare it. This control is about routing and log
                // retention, which launch no `log-diff`; the post-pass has
                // its own test
                // (the_parity_post_pass_changes_no_determinism_output_and_runs_no_guest).
                .env("E2E_PARITY_POST_PASS", "0")
                .env("DAGRUN_TEST_COUNTS_PATH", path.join("counts.json"))
                .output()
                .unwrap();
            fs::write(path.join("child.stdout"), &result.stdout).unwrap();
            fs::write(path.join("child.stderr"), &result.stderr).unwrap();
            assert!(
                result.status.success(),
                "{scenario}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            let results = fs::read_to_string(path.join("results.jsonl")).unwrap();
            // No row carries the retired cross-backend report.
            assert!(!results.contains("\"backend_parity\""), "{scenario}");
            let rows = results
                .lines()
                .map(|line| {
                    serde_json::from_str::<hermit_manifest_plan::runner::CellResult>(line).unwrap()
                })
                .collect::<Vec<_>>();
            let calls = fs::read_to_string(path.join("invocations"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
            // verify@ptrace, verify@kvm and custom@kvm, each exactly once.
            assert_eq!(rows.len(), 3, "{scenario}: {rows:#?}");
            for row in &rows {
                row.require_current_classification().unwrap();
                row.require_current_timeout_policy().unwrap();
                assert_eq!(row.execution_cpu_timeout_seconds, Some(5));
                assert_eq!(row.execution_wall_timeout_seconds, Some(10));
                assert_eq!(row.outcome, "PASS", "{scenario}: {row:#?}");
                assert_eq!(row.result, Some(ObservedResult::Pass), "{scenario}");
                assert_eq!(row.attempts.len(), 1, "{scenario}: {row:#?}");
            }
            for (mode, backend) in [("verify", "ptrace"), ("verify", "kvm"), ("custom", "kvm")] {
                assert_eq!(
                    rows.iter()
                        .filter(|row| row.mode == mode && row.backend.as_deref() == Some(backend))
                        .count(),
                    1,
                    "{scenario}: {mode}@{backend}"
                );
            }
            let kinds = calls
                .iter()
                .map(|call| {
                    format!(
                        "{}:{}:{}",
                        call["kind"].as_str().unwrap(),
                        call["backend"].as_str().unwrap_or("-"),
                        call["reference"].as_bool().unwrap_or(false)
                    )
                })
                .collect::<BTreeSet<_>>();
            // One guest run per cell and no `log-diff` of either kind. The
            // ptrace golden-log normalization, a fourth, single-log call here
            // until https://github.com/rrnewton/hermit/issues/3301, was
            // removed: parity canonicalizes run 1's log when it compares. The
            // fake still records a single-log call as "normalize" and a
            // two-log comparison as "compare", so either would fail this.
            assert_eq!(calls.len(), 3, "{scenario}: {calls:#?}");
            assert_eq!(
                kinds,
                BTreeSet::from([
                    "custom:kvm:false".to_string(),
                    "run:kvm:false".to_string(),
                    "run:ptrace:false".to_string(),
                ]),
                "{scenario}: no ptrace reference run and no log-diff comparison"
            );
            assert!(
                calls
                    .iter()
                    .all(|call| call["kind"] != "normalize" && call["kind"] != "compare"),
                "{scenario}: E2E_KEEP_VERIFY_LOGS launched a log-diff: {calls:#?}"
            );
        }
    }

    /// A verify attempt whose only failure is Hermit's typed precise-timer
    /// overshoot earns one SKID-RETRY, which is counted on the summary line,
    /// named in summary.json and kept as a row; a second overshoot fails the
    /// cell loudly (<https://github.com/rrnewton/hermit/issues/1845>). An
    /// overshoot report without its stderr markers, a run with `--no-retry`,
    /// a divergence that merely prints the markers, and an overshoot beside a
    /// rejected guest disposition (run 1 exited 1 so Hermit wrote its skid
    /// report without a comparison, or a compared run 2 died from a signal)
    /// are never SKID-RETRIES. Neither is an overshoot whose compared runs
    /// printed something other than the cell's declared `expected_stdout`:
    /// that attempt keeps the skid infrastructure ERROR, with a reason naming
    /// both, is never retried, and the cell stays red.
    /// The cap line names the outcome the whole history selects: after a
    /// divergence and then an overshoot, that is the divergence's FAIL.
    /// Imported (`E2E_IMPORT_RESULTS`), the skid-then-pass history earns the
    /// same counted SKID-RETRY and selects the same PASS as executed, with no
    /// producer retry dropped. An imported skid row its retained evidence does
    /// not decide as typed skid-only (a report that does not match its digest,
    /// another stdout declaration, a vacuous comparison, other relaxations, an
    /// undecided SaBRe path), and an imported ERROR that is not a skid at all,
    /// ends the history: that ERROR is the cell's verdict and the PASS after
    /// it is dropped.
    #[test]
    fn a_skid_overshoot_only_verify_attempt_earns_one_counted_skid_retry() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::process::Command;
        use std::process::ExitCode;

        use hermit_manifest_plan::runner::Selection;
        use serde_json::json;

        const CHILD: &str = "HERMIT_SKID_RETRY_FIXTURE";
        // Where an import-mode child publishes, beside the executed run's
        // output it imports.
        const IMPORT_OUT: &str = "HERMIT_SKID_RETRY_FIXTURE_IMPORT_OUT";
        const TEST: &str =
            "tests::a_skid_overshoot_only_verify_attempt_earns_one_counted_skid_retry";
        // The commit the fixture snapshot names. Each child runs against the
        // fixture as a Git-less source snapshot (`--repo-root` plus
        // `--source-sha`), never against this checkout: describing the
        // checkout runs `git status` over every tracked file, and in the
        // validation container (files owned by another uid, Git metadata
        // mounted read-only) that rehashes the whole tree in each of this
        // test's 28 child runs. That alone exhausted the test's 22 s CPU
        // budget (https://github.com/rrnewton/hermit/issues/1845).
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        if let Some(fixture) = std::env::var_os(CHILD) {
            let fixture = PathBuf::from(fixture);
            let out = std::env::var_os(IMPORT_OUT).map_or_else(|| fixture.clone(), PathBuf::from);
            let mut values = vec![
                "--category".to_string(),
                "parity".into(),
                "--ci-only".into(),
                "--prebuilt".into(),
                "--repo-root".into(),
                fixture.display().to_string(),
                "--source-sha".into(),
                SHA.into(),
                "--jobs".into(),
                "1".into(),
                "--results".into(),
                out.join("results.jsonl").display().to_string(),
                "--junit".into(),
                out.join("junit.xml").display().to_string(),
            ];
            if fs::read_to_string(fixture.join("scenario")).unwrap() == "skid-no-retry" {
                values.push("--no-retry".into());
            }
            let args = parse(values.into_iter());
            validate_args("run", &args);
            let manifests = ManifestSet::load(&fixture).unwrap();
            let code = super::run(&fixture, &manifests, &args);
            std::process::exit(if code == ExitCode::SUCCESS { 0 } else { 1 });
        }

        let output = json!({"exit_code": 0, "signal": null,
            "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stdout_bytes": 0, "stderr_bytes": 0});
        let matched = json!({
            "verified": true, "bitwise_parity": true, "verdict": "matched",
            "no_result_reason": null, "infrastructure_error": null,
            "comparison": {"strictness": "canonical", "display_name": "BitwiseInfoV1",
                "compare_logs": true, "compare_io_buffers": true, "log_scope": "info",
                "record_envelope": "all_records_v1", "virtualize_time": true,
                "strip_lines": false, "canonicalize_addresses": true, "full_trace": true,
                "exact_remainder": true, "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines": false, "skip_commit": false, "skip_detlog": false},
            "compared_log_messages": {"left": 2, "right": 2},
            "compared_outputs": {"left": output, "right": output},
            "guest_exit_code": 0, "guest_signal": null,
            "first_divergent_scheduler_turn": null, "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null, "first_divergent_syscall": null,
            "first_divergent_left_message": null, "first_divergent_right_message": null
        });
        let mut skid = matched.clone();
        skid["verified"] = json!(false);
        skid["bitwise_parity"] = json!(false);
        skid["verdict"] = json!("infrastructure_error");
        skid["infrastructure_error"] = json!({"kind": "skid_overshoot", "count": 2});
        let mut diverged = matched.clone();
        diverged["verified"] = json!(false);
        diverged["bitwise_parity"] = json!(false);
        diverged["verdict"] = json!("diverged");
        diverged["first_divergent_scheduler_turn"] = json!(4);
        diverged["first_divergent_virtual_nanoseconds"] = json!(7);
        diverged["first_divergent_record"] = json!(9);
        diverged["first_divergent_syscall"] = json!(2);
        diverged["first_divergent_left_message"] = json!("left");
        diverged["first_divergent_right_message"] = json!("right");
        // Hermit's skid report when run 1 exited 1, which the verify policy
        // rejects: no comparison, and run 1's rejected status.
        let rejected = json!({
            "verified": false, "bitwise_parity": false, "verdict": "infrastructure_error",
            "no_result_reason": null,
            "infrastructure_error": {"kind": "skid_overshoot", "count": 2},
            "comparison": null, "compared_log_messages": null, "compared_outputs": null,
            "runtime": null, "guest_exit_code": 1, "guest_signal": null,
            "first_divergent_scheduler_turn": null, "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null, "first_divergent_syscall": null,
            "first_divergent_left_message": null, "first_divergent_right_message": null
        });
        // A completed comparison whose run 2 died from SIGSEGV.
        let mut crashed = skid.clone();
        crashed["compared_outputs"]["right"]["exit_code"] = json!(null);
        crashed["compared_outputs"]["right"]["signal"] = json!(11);
        for report in [&matched, &skid, &diverged, &rejected, &crashed] {
            hermit_manifest_plan::canonical_verdict::VerificationReport::from_current_json_slice(
                &serde_json::to_vec(report).unwrap(),
            )
            .unwrap();
        }

        let manifests = ManifestSet::load(&super::root(None)).unwrap();
        let real_cell = |test: &str| {
            manifests
                .select(&Selection {
                    test: Some(test.into()),
                    mode: Some("verify".into()),
                    backend: Some("ptrace".into()),
                    population: Some(hermit_manifest_plan::runner::Population::Required),
                    ..Selection::default()
                })
                .unwrap()
                .remove(0)
        };

        // (scenario, harness exit success, attempts launched, SKID-RETRY
        // count, PRODUCT-RETRY count)
        for (scenario, succeeds, launched, skid_retries, product_retries) in [
            ("skid-then-match", true, 2, 1, 0),
            ("skid-then-match-sabre", true, 2, 1, 0),
            ("skid-twice", false, 2, 1, 0),
            ("skid-no-retry", false, 1, 0, 0),
            ("unmarked-skid", false, 1, 0, 0),
            ("diverged-marked", false, 2, 0, 1),
            ("diverged-then-skid", false, 2, 0, 1),
            ("unmarked-diverged-then-match", true, 2, 0, 1),
            ("rejected-guest-skid", false, 1, 0, 0),
            ("crashed-operand-skid", false, 1, 0, 0),
            ("skid-wrong-stdout", false, 1, 0, 0),
        ] {
            let fixture = std::env::temp_dir().join(format!(
                "hermit-harness-skid-retry-{}-{:?}-{scenario}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&fixture);
            fs::create_dir(&fixture).unwrap();
            let path = fixture.as_path();
            // The SaBRe scenario runs the same history on the one backend
            // whose rows carry execution-path evidence.
            let backend = if scenario == "skid-then-match-sabre" {
                "sabre"
            } else {
                "ptrace"
            };
            fs::write(path.join("scenario"), scenario).unwrap();
            for (name, report) in [
                ("matched", &matched),
                ("skid", &skid),
                ("diverged", &diverged),
                ("rejected", &rejected),
                ("crashed", &crashed),
            ] {
                fs::write(
                    path.join(format!("{name}.json")),
                    serde_json::to_vec(report).unwrap(),
                )
                .unwrap();
            }
            let manifests_dir = path.join("tests/e2e/manifests");
            fs::create_dir_all(&manifests_dir).unwrap();
            fs::write(
                manifests_dir.join("defaults.yaml"),
                "schema: 3\ntimeout_seconds: 10\ncpu_timeout_seconds: 5\n",
            )
            .unwrap();
            let disabled = json!({"ci": false, "backends_enabled": [], "backends_disabled": {
                "ptrace": "Not selected by this control", "dbt": "Not selected by this control",
                "kvm": "Not selected by this control", "sabre": "Not selected by this control",
                "liteinst": "Not selected by this control",
                "in-guest-trap": "Not selected by this control"
            }});
            let mut verify = if backend == "sabre" {
                json!({"ci": true, "backends_enabled": ["sabre"],
                    "backends_disabled": {"ptrace": "Not selected", "kvm": "Not selected", "dbt": "Not selected", "liteinst": "Not selected", "in-guest-trap": "Not selected"}})
            } else {
                json!({"ci": true, "backends_enabled": ["ptrace"],
                    "backends_disabled": {"kvm": "Not selected", "dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected", "in-guest-trap": "Not selected"}})
            };
            if scenario == "skid-wrong-stdout" {
                // Every fixture report records empty stdout from both runs, so
                // the skid attempt violates this declaration; no retry follows.
                verify["expected_stdout"] = json!({"ptrace": "skid-control-ok\n"});
            }
            fs::write(
                manifests_dir.join("parity.yaml"),
                serde_json::to_vec(&json!({
                    "schema": 3, "bucket": "parity", "test": [{
                        "id": "parity/skid", "description": "Precise-timer overshoot retry control",
                        "lane": "portable", "occasional": false, "direct": ["/bin/true"],
                        "observation": {"status": true, "stdout": true, "stderr": true},
                        "modes": {
                            "verify": verify,
                            "naked": {"ci": false, "backends_enabled": [],
                                "backends_disabled": {"native": "Not selected by this CI control"}},
                            "chaos": disabled, "replay": disabled, "custom": disabled
                        }
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
            let hermit = path.join("fake-hermit");
            fs::write(
                &hermit,
                r#"#!/usr/bin/python3
import json,os,pathlib,sys
root=pathlib.Path(__file__).parent
a=sys.argv[1:]
if '--help' in a:
 print('--verify-strict');sys.exit(0)
if 'run' in a:a=a[a.index('run'):]
if not a or a[0]!='run':sys.exit(0)
calls=root/'invocations'
n=len(calls.read_text().splitlines()) if calls.exists() else 0
with calls.open('a') as f:f.write(json.dumps(a)+'\n')
plan={'skid-then-match':['skid','matched'],'skid-then-match-sabre':['skid','matched'],
 'skid-twice':['skid','skid'],
 'skid-no-retry':['skid'],'unmarked-skid':['unmarked'],'diverged-marked':['diverged','diverged'],
 'diverged-then-skid':['diverged','skid'],'rejected-guest-skid':['rejected'],
 'crashed-operand-skid':['crashed'],'skid-wrong-stdout':['skid','matched'],
 'unmarked-diverged-then-match':['unmarked-diverged','matched']}
kind=plan[(root/'scenario').read_text()][n]
report=pathlib.Path(a[a.index('--verify-json')+1])
logdir=pathlib.Path(a[a.index('--verify-log-dir')+1])
logdir.mkdir(parents=True,exist_ok=True)
(logdir/'run1_log_fixture.log').write_text('INFO detcore: shared\nINFO detcore: complete\n')
report.write_bytes((root/({'unmarked':'skid','unmarked-diverged':'diverged'}.get(kind,kind)+'.json')).read_bytes())
evidence=os.environ.get('HERMIT_SABRE_PATH_EVIDENCE')
if evidence:
 line='{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}\n'
 pathlib.Path(evidence).write_text(line+line)
if kind in ('skid','diverged','rejected','crashed'):
 sys.stderr.write('HERMIT_SKID_OVERSHOOT rcb_actual=39951476 rcb_target=39950647 skid_margin=1000 overshoot=829\n'
  'HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=2\n')
sys.exit({'skid':122,'unmarked':122,'rejected':122,'crashed':122,'matched':0,'diverged':1,'unmarked-diverged':1}[kind])
"#,
            )
            .unwrap();
            fs::set_permissions(&hermit, fs::Permissions::from_mode(0o755)).unwrap();
            let result = Command::new("timeout")
                .args(["--kill-after=2s", "60s"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env(CHILD, path)
                .env("HERMIT_BIN", &hermit)
                .env("E2E_RESULT_ROOT", path.join("artifacts"))
                .env("E2E_BUILD_ROOT", path.join("build"))
                .env("E2E_RUN_ID", "skid-retry-control")
                .env("E2E_MACHINE_SHORTNAME", "skid-control")
                .env("E2E_KERNEL_VERSION", "skid-control")
                .env("E2E_KEEP_VERIFY_LOGS", "1")
                .env("DAGRUN_TEST_COUNTS_PATH", path.join("counts.json"))
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
            assert_eq!(
                result.status.code(),
                Some(if succeeds { 0 } else { 1 }),
                "{scenario}: {stdout}\n{stderr}"
            );
            let calls = fs::read_to_string(path.join("invocations")).unwrap();
            assert_eq!(calls.lines().count(), launched, "{scenario}: {calls}");
            let rows = fs::read_to_string(path.join("results.jsonl"))
                .unwrap()
                .lines()
                .map(|line| {
                    serde_json::from_str::<hermit_manifest_plan::runner::CellResult>(line).unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), launched, "{scenario}: {rows:#?}");
            for (position, row) in rows.iter().enumerate() {
                assert_eq!(row.attempt, position as u64 + 1, "{scenario}");
                row.require_current_classification().unwrap();
                // The child described the fixture snapshot, not this checkout.
                assert_eq!(
                    (row.hermit_sha.as_str(), row.source_tree_dirty),
                    (SHA, false),
                    "{scenario}"
                );
            }
            let summary: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("summary.json")).unwrap()).unwrap();
            assert_eq!(summary["skid_retries"], json!(skid_retries), "{scenario}");
            assert!(
                stdout.contains(&format!("; SKID-RETRY count {skid_retries}\n")),
                "{scenario}: the summary line must carry the SKID-RETRY count: {stdout}"
            );
            // Every retry is counted, not only a SKID-RETRY.
            assert_eq!(
                summary["product_retries"],
                json!(product_retries),
                "{scenario}"
            );
            assert_eq!(summary["host_input_retries"], json!(0), "{scenario}");
            assert!(
                stdout.contains(&format!(
                    "test-harness: PRODUCT-RETRY count {product_retries} ({} final PASS); HOST-INPUT-RETRY count 0\n",
                    usize::from(scenario == "unmarked-diverged-then-match")
                )),
                "{scenario}: the summary must carry the PRODUCT-RETRY count: {stdout}"
            );
            let first = &rows[0];
            match scenario {
                "skid-then-match" | "skid-then-match-sabre" | "skid-twice" => {
                    let final_outcome = if succeeds { "PASS" } else { "ERROR" };
                    assert_eq!(first.outcome, "ERROR", "{scenario}");
                    assert_eq!(rows[1].outcome, final_outcome, "{scenario}");
                    assert_eq!(
                        summary["skid_retry_cells"],
                        json!([{
                            "test": "parity/skid", "mode": "verify", "backend": backend,
                            "attempt": 1, "overshoot_reports": 2, "final_outcome": final_outcome,
                        }]),
                        "{scenario}"
                    );
                    assert!(
                        stdout.contains(&format!(
                            "ERROR parity/skid (verify/{backend}) [SKID-RETRY: attempt 1 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)) and nothing else; retrying this cell only]"
                        )),
                        "{scenario}: {stdout}"
                    );
                    assert!(
                        stdout.contains(&format!(
                            "test-harness: SKID-RETRY parity/skid (verify/{backend}): attempt 1 recorded 2 HERMIT_SKID_OVERSHOOT report(s) and was retried; final outcome {final_outcome}"
                        )),
                        "{scenario}: {stdout}"
                    );
                    assert_eq!(
                        stdout.contains(&format!(
                            "SKID-RETRY LIMIT parity/skid (verify/{backend}): attempt 2 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)); the attempt cap is reached, no further retry is made, and the cell's selected outcome is ERROR (attempt 2)\n"
                        )),
                        !succeeds,
                        "{scenario}: the cap must be loud exactly when it is reached: {stdout}"
                    );
                    assert!(
                        !stdout.contains("SKID-RETRY NOT MADE"),
                        "{scenario}: {stdout}"
                    );
                    assert_eq!(
                        summary["errors"],
                        json!(usize::from(!succeeds)),
                        "{scenario}"
                    );
                    // A history that ends at a skid attempt below the cap
                    // names the missing retry, and why, without claiming the
                    // cap was reached.
                    assert_eq!(
                        super::unretried_skid_line(
                            "parity/skid",
                            "verify",
                            Some(backend),
                            1,
                            2,
                            &rows[..1],
                            false
                        ),
                        format!(
                            "SKID-RETRY NOT MADE parity/skid (verify/{backend}): attempt 1 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)), but this run has no later attempt of the cell; the cell's selected outcome is ERROR (attempt 1)"
                        ),
                        "{scenario}"
                    );
                    assert_eq!(
                        super::unretried_skid_line(
                            "parity/skid",
                            "verify",
                            Some(backend),
                            1,
                            2,
                            &rows[..1],
                            true
                        ),
                        format!(
                            "SKID-RETRY NOT MADE parity/skid (verify/{backend}): attempt 1 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)), but it is an imported attempt and its producer recorded no later attempt of the cell; the cell's selected outcome is ERROR (attempt 1)"
                        ),
                        "{scenario}"
                    );
                }
                "diverged-then-skid" => {
                    // The retry was earned by the product failure, so it is
                    // not a SKID-RETRY; the capped skid attempt is still named,
                    // and the selected outcome is the earlier FAIL, not ERROR.
                    assert_eq!(first.outcome, "FAIL", "{scenario}");
                    assert_eq!(rows[1].outcome, "ERROR", "{scenario}");
                    assert_eq!(summary["skid_retry_cells"], json!([]), "{scenario}");
                    assert!(!stdout.contains("SKID-RETRY:"), "{scenario}: {stdout}");
                    assert!(
                        stdout.contains("[attempt 1 of at most 2; retrying this cell only]"),
                        "{scenario}: {stdout}"
                    );
                    assert!(
                        stdout.contains(
                            "SKID-RETRY LIMIT parity/skid (verify/ptrace): attempt 2 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)); the attempt cap is reached, no further retry is made, and the cell's selected outcome is FAIL (attempt 1)\n"
                        ),
                        "{scenario}: {stdout}"
                    );
                    assert!(
                        !stdout.contains("test-harness: SKID-RETRY "),
                        "{scenario}: {stdout}"
                    );
                }
                _ => {
                    assert_eq!(summary["skid_retry_cells"], json!([]), "{scenario}");
                    assert!(!stdout.contains("SKID-RETRY:"), "{scenario}: {stdout}");
                    assert!(!stdout.contains("SKID-RETRY LIMIT"), "{scenario}: {stdout}");
                    assert!(
                        !stdout.contains("SKID-RETRY NOT MADE"),
                        "{scenario}: {stdout}"
                    );
                    assert!(
                        !stdout.contains("test-harness: SKID-RETRY "),
                        "{scenario}: {stdout}"
                    );
                }
            }
            match scenario {
                "skid-no-retry" => {
                    // The same row would have earned a retry in a framework run.
                    assert_eq!(
                        super::attempt_retry_cause(
                            super::Retries::Framework,
                            &real_cell("c-programs/getrusage-self-accounting"),
                            first
                        ),
                        Some(super::RetryCause::SkidOvershoot { reports: 2 })
                    );
                    assert_eq!(
                        super::attempt_retry_cause(
                            super::Retries::Off,
                            &real_cell("c-programs/getrusage-self-accounting"),
                            first
                        ),
                        None
                    );
                    // A row that declares exit 7 and whose compared runs both
                    // exited 7 is skid-only by its own declaration, but the
                    // real cell declares no expected exit, so it chooses none.
                    let mut sevens = skid.clone();
                    sevens["guest_exit_code"] = json!(7);
                    sevens["compared_outputs"]["left"]["exit_code"] = json!(7);
                    sevens["compared_outputs"]["right"]["exit_code"] = json!(7);
                    let mut redeclared = first.clone();
                    redeclared.expected_guest_exit =
                        Some(hermit_manifest_plan::runner::ExpectedGuestExit {
                            code: Some(7),
                            signal: None,
                            reason: "an imported row's own declaration".into(),
                        });
                    let sevens = serde_json::to_string(&sevens).unwrap();
                    redeclared.attempts[0].verification_report_sha256 = Some({
                        use sha2::Digest;
                        format!("{:x}", sha2::Sha256::digest(sevens.as_bytes()))
                    });
                    redeclared.attempts[0].verification_report = Some(sevens);
                    // The executor writes `--verify-allow=failure` after
                    // `--verify` for a cell that declares an expected exit,
                    // and records the shell command and the row's copies from
                    // that argv; without it the command line is not one a
                    // declaring cell runs.
                    assert_eq!(
                        super::skid_overshoot_only_reports(&redeclared),
                        None,
                        "{redeclared:#?}"
                    );
                    for attempt in &mut redeclared.attempts {
                        let verify = attempt
                            .argv
                            .iter()
                            .position(|arg| arg == "--verify")
                            .unwrap();
                        attempt
                            .argv
                            .insert(verify + 1, "--verify-allow=failure".into());
                        attempt.shell_command = hermit_manifest_plan::runner::shell_command(
                            &attempt.cwd,
                            &attempt.env,
                            &attempt.argv,
                        );
                    }
                    redeclared.argv = redeclared.attempts[0].argv.clone();
                    redeclared.shell_command = redeclared.attempts[0].shell_command.clone();
                    redeclared.effective_args = redeclared.argv[1..].to_vec();
                    assert_eq!(
                        hermit_manifest_plan::runner::retained_verify_invocation_error(&redeclared),
                        None
                    );
                    assert_eq!(
                        super::skid_overshoot_only_reports(&redeclared),
                        Some(2),
                        "{redeclared:#?}"
                    );
                    assert_eq!(
                        super::attempt_retry_cause(
                            super::Retries::Framework,
                            &real_cell("c-programs/getrusage-self-accounting"),
                            &redeclared
                        ),
                        None
                    );
                    // Likewise a row recorded under a stdout declaration the
                    // real cell does not make: its compared runs meet its own
                    // (an empty exact stdout), so the skid rule accepts it,
                    // but the real cell declares no stdout assertion.
                    let mut restated = first.clone();
                    restated.declared_stdout = Some(hermit_manifest_plan::runner::DeclaredStdout {
                        exact: Some(String::new()),
                        contains: None,
                    });
                    assert_eq!(
                        super::skid_overshoot_only_reports(&restated),
                        Some(2),
                        "{restated:#?}"
                    );
                    assert_eq!(
                        super::attempt_retry_cause(
                            super::Retries::Framework,
                            &real_cell("c-programs/getrusage-self-accounting"),
                            &restated
                        ),
                        None
                    );
                    // A row whose report is not the bytes its recorded digest
                    // names (an imported or edited row) earns no retry: the
                    // skid rule decides nothing from such a report.
                    for (label, digest) in [("missing", None), ("mismatched", Some("0".repeat(64)))]
                    {
                        let mut undigested = first.clone();
                        undigested.attempts[0].verification_report_sha256 = digest;
                        assert_eq!(
                            super::attempt_retry_cause(
                                super::Retries::Framework,
                                &real_cell("c-programs/getrusage-self-accounting"),
                                &undigested
                            ),
                            None,
                            "a {label} report digest: {undigested:#?}"
                        );
                    }
                    // A single-attempt compatibility cell keeps one attempt.
                    assert_eq!(
                        super::attempt_retry_cause(
                            super::Retries::Framework,
                            &real_cell("compat/cat"),
                            first
                        ),
                        None
                    );
                }
                "unmarked-skid" => {
                    assert_eq!(first.outcome, "ERROR", "{scenario}");
                    assert_eq!(first.error_kind.as_deref(), Some("infrastructure"));
                }
                "rejected-guest-skid" | "crashed-operand-skid" => {
                    // The same typed skid ERROR as an earned retry, refused
                    // only for its guest disposition, and the real cell would
                    // refuse it too.
                    assert_eq!(first.outcome, "ERROR", "{scenario}");
                    assert_eq!(first.error_kind.as_deref(), Some("infrastructure"));
                    assert_eq!(
                        first.reason.as_deref(),
                        Some("verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)"),
                        "{scenario}"
                    );
                    assert_eq!(summary["errors"], json!(1), "{scenario}");
                    assert_eq!(
                        super::attempt_retry_cause(
                            super::Retries::Framework,
                            &real_cell("c-programs/getrusage-self-accounting"),
                            first
                        ),
                        None,
                        "{scenario}"
                    );
                }
                "diverged-marked" => {
                    // The ordinary product-failure retry, never a SKID-RETRY.
                    assert!(rows.iter().all(|row| row.outcome == "FAIL"), "{rows:#?}");
                    assert!(
                        stdout.contains("[attempt 1 of at most 2; retrying this cell only]"),
                        "{stdout}"
                    );
                    // Counted and located, though the overshoot marker it
                    // also carries is not what retried it.
                    assert_eq!(
                        summary["product_retry_cells"],
                        json!([{
                            "test": "parity/skid", "mode": "verify", "backend": "ptrace",
                            "attempt": 1, "outcome": "FAIL", "result": "determinism-failure",
                            "first_divergent_scheduler_turn": 4,
                            "first_divergent_record": 9, "first_divergent_syscall": 2,
                            "final_outcome": "FAIL",
                        }]),
                        "{scenario}"
                    );
                    assert!(
                        stdout.contains(
                            "test-harness: PRODUCT-RETRY parity/skid (verify/ptrace): attempt 1 was FAIL (determinism-failure turn=4 rec=9 sys=2) and was retried; final outcome FAIL\n"
                        ),
                        "{stdout}"
                    );
                }
                "skid-wrong-stdout" => {
                    // A typed, marked overshoot whose compared runs both
                    // printed something other than the declared stdout: the
                    // skid infrastructure ERROR every row reader accepts, with
                    // a reason that names the overshoot and the violated
                    // assertion, never retried and never a SKID-RETRY. The row
                    // records the declaration it was decided under.
                    let reason = format!(
                        "verification recorded 2 HERMIT_SKID_OVERSHOOT report(s), and its compared runs violate a declared stdout assertion: {}",
                        hermit_manifest_plan::runner::expected_stdout_mismatch_reason(
                            "first",
                            0,
                            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                            16,
                            "4aab145057a66c842ce881dbe313e2caf4bf7e5e57c8bf60b07ffc097e59f20a",
                        )
                    );
                    assert_eq!(
                        (
                            first.outcome.as_str(),
                            first.error_kind.as_deref(),
                            first.reason.as_deref(),
                            first.failure_class,
                        ),
                        (
                            "ERROR",
                            Some("infrastructure"),
                            Some(reason.as_str()),
                            Some(
                                hermit_manifest_plan::runner::FailureClass::UnderstoodInfrastructureFailure
                            ),
                        ),
                        "{rows:#?}"
                    );
                    assert_eq!(first.attempts.len(), 1, "{rows:#?}");
                    assert_eq!(
                        first.declared_stdout,
                        Some(hermit_manifest_plan::runner::DeclaredStdout {
                            exact: Some("skid-control-ok\n".into()),
                            contains: None,
                        }),
                        "{rows:#?}"
                    );
                    assert_eq!(summary["errors"], json!(1), "{scenario}");
                    assert_eq!(super::skid_overshoot_only_reports(first), None);
                    // The same attempt with the plain skid reason, as an older
                    // producer would have labelled it, is still refused: the
                    // row's recorded declaration is re-decided against the
                    // report's digests.
                    let mut relabelled = first.clone();
                    let skid_reason = "verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)";
                    relabelled.reason = Some(skid_reason.into());
                    relabelled.attempts[0].reason = Some(skid_reason.into());
                    assert_eq!(
                        super::skid_overshoot_only_reports(&relabelled),
                        None,
                        "{relabelled:#?}"
                    );
                    let mut undeclared = relabelled.clone();
                    undeclared.declared_stdout =
                        Some(hermit_manifest_plan::runner::DeclaredStdout::default());
                    assert_eq!(
                        super::skid_overshoot_only_reports(&undeclared),
                        Some(2),
                        "the relabelled row differs from a qualifying one only in its declaration"
                    );
                    assert!(!stdout.contains("retrying this cell only"), "{stdout}");
                }
                "unmarked-diverged-then-match" => {
                    // A divergence with no overshoot marker, then a match: the
                    // framework retry selects PASS, and the count says so.
                    assert_eq!(first.outcome, "FAIL", "{scenario}");
                    assert_eq!(rows[1].outcome, "PASS", "{scenario}");
                    assert_eq!(summary["passed"], json!(1), "{scenario}");
                    assert_eq!(
                        summary["product_retry_cells"],
                        json!([{
                            "test": "parity/skid", "mode": "verify", "backend": "ptrace",
                            "attempt": 1, "outcome": "FAIL", "result": "determinism-failure",
                            "first_divergent_scheduler_turn": 4,
                            "first_divergent_record": 9, "first_divergent_syscall": 2,
                            "final_outcome": "PASS",
                        }]),
                        "{scenario}"
                    );
                    assert!(
                        stdout.contains(
                            "test-harness: PRODUCT-RETRY parity/skid (verify/ptrace): attempt 1 was FAIL (determinism-failure turn=4 rec=9 sys=2) and was retried; final outcome PASS\n"
                        ),
                        "{stdout}"
                    );
                    assert!(!stdout.contains("SKID-RETRY:"), "{scenario}: {stdout}");
                }
                _ => {}
            }
            if scenario.starts_with("skid-then-match") {
                // The same history imported (`E2E_IMPORT_RESULTS`) as a Buck
                // run leaves it: each attempt is its own producer execution
                // with its own run id and CPU observations bound to that
                // execution's outer attempt 1, from a clean tree, and each has
                // an evidence-complete record. The importer keeps a producer's
                // later attempt exactly when this run's retry decision
                // (`attempt_retry_cause`, the one an executed run applies to
                // its own attempt) retries the attempt before it, and that
                // decision re-decides a skid row from the report bytes it
                // retains. So the intact history earns the same counted
                // SKID-RETRY as executed, publishes both of its producer's
                // rows and selects the PASS, with no producer retry dropped.
                //
                // A skid row whose own evidence does not decide it as typed
                // skid-only earns no retry: a report that does not match its
                // digest, a stdout declaration other than the cell's, a
                // comparison that compared no log messages, relaxations the
                // cell's recipe does not select, or a SaBRe path its retained
                // evidence does not decide. Neither does an infrastructure
                // ERROR that is no skid at all. Each ends the history: that
                // ERROR is the cell's verdict, the producer's PASS is dropped
                // and counted, and no line names a retry. A skid row whose
                // producer recorded no later attempt is the verdict as well,
                // and its line says why.
                //
                // The importer re-decides the retried row, never a PASS: a
                // PASS is published under the same rules at any attempt. So a
                // PASS after the retried skid row that its retained evidence
                // contradicts (a missing or wrong report digest, no report, a
                // correctly hashed diverged report, relaxations the cell's
                // recipe does not select, or SaBRe path evidence that is
                // missing, wrong, or names a ptrace fallback or a ptrace
                // command line) is imported a second time alone, as its
                // producer's only attempt, and must end exactly as it does
                // there: a history that recovered from a skid ends no better
                // than one that never overshot.
                enum Expect {
                    /// The skid row earns its retry, and the producer's PASS
                    /// after it is the verdict.
                    Retried,
                    /// The skid row earns no retry: it ends the history.
                    Ended,
                    /// The producer recorded only the skid attempt.
                    Unretried,
                }
                let sha256 = |text: &str| {
                    use sha2::Digest;
                    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
                };
                let diverged_raw = serde_json::to_string(&diverged).unwrap();
                let mut vacuous = skid.clone();
                vacuous["compared_log_messages"] = json!({"left": 0, "right": 0});
                let vacuous_raw = serde_json::to_string(&vacuous).unwrap();
                let clean_path = r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}"#;
                let fallback_path = r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":1,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}"#;
                let fallback_evidence = format!("{clean_path}\n{fallback_path}\n");
                if backend == "sabre" {
                    // The executed history is a genuine SaBRe one: each row's
                    // recorded path is the eligible summary of the two clean
                    // records its attempt retained.
                    for row in &rows {
                        assert_eq!(
                            row.execution_path.as_ref().map(|path| &path["eligible"]),
                            Some(&json!(true)),
                            "{rows:#?}"
                        );
                        assert_eq!(
                            hermit_manifest_plan::runner::retained_execution_path_error(row),
                            None
                        );
                    }
                }
                // Import `imported` as the producer rows of a bucket and run
                // this test's child on it; the consumer's run id is
                // `skid-import-consumer`.
                let import = |name: &str, imported: &[CellResult]| {
                    let import = path.join(name).join("import");
                    let bucket = hermit_manifest_plan::imported_results::bucket_dir(
                        &import, "portable", "parity",
                    );
                    fs::create_dir_all(&bucket).unwrap();
                    fs::write(
                        bucket.join("results.jsonl"),
                        imported
                            .iter()
                            .map(|row| serde_json::to_string(row).unwrap() + "\n")
                            .collect::<String>(),
                    )
                    .unwrap();
                    let evidence = imported
                        .iter()
                        .map(|row| {
                            json!({"test": row.test, "mode": row.mode,
                                "backend": row.backend, "run_id": row.run_id})
                        })
                        .collect::<Vec<_>>();
                    fs::write(
                        bucket.join("summary.json"),
                        serde_json::to_vec(&json!({"evidence_complete_executions": evidence}))
                            .unwrap(),
                    )
                    .unwrap();
                    let out = path.join(name).join("out");
                    fs::create_dir_all(&out).unwrap();
                    let result = Command::new("timeout")
                        .args(["--kill-after=2s", "60s"])
                        .arg(std::env::current_exe().unwrap())
                        .args(["--exact", TEST, "--nocapture"])
                        .env_clear()
                        .env("PATH", "/usr/bin:/bin")
                        .env(CHILD, path)
                        .env(IMPORT_OUT, &out)
                        .env("E2E_IMPORT_RESULTS", &import)
                        .env("HERMIT_BIN", &hermit)
                        .env("E2E_RESULT_ROOT", path.join(name).join("artifacts"))
                        .env("E2E_BUILD_ROOT", path.join("build"))
                        .env("E2E_RUN_ID", "skid-import-consumer")
                        .env("E2E_MACHINE_SHORTNAME", "skid-control")
                        .env("E2E_KERNEL_VERSION", "skid-control")
                        .env("DAGRUN_TEST_COUNTS_PATH", out.join("counts.json"))
                        .output()
                        .unwrap();
                    let context = format!(
                        "{scenario} {name}: {}\n{}",
                        String::from_utf8_lossy(&result.stdout),
                        String::from_utf8_lossy(&result.stderr)
                    );
                    // Import mode runs no cell. The log records `hermit run`
                    // alone, so the import's `host-capabilities --json`
                    // question adds no line to the producer's launches.
                    let calls = fs::read_to_string(path.join("invocations")).unwrap();
                    assert_eq!(calls.lines().count(), launched, "{context}");
                    let published = fs::read_to_string(out.join("results.jsonl"))
                        .unwrap()
                        .lines()
                        .map(|line| {
                            serde_json::from_str::<hermit_manifest_plan::runner::CellResult>(line)
                                .unwrap()
                        })
                        .collect::<Vec<_>>();
                    let summary: serde_json::Value =
                        serde_json::from_slice(&fs::read(out.join("summary.json")).unwrap())
                            .unwrap();
                    (result, published, summary, context)
                };
                // A producer row as this run republishes it.
                let rebound = |row: &CellResult| {
                    let mut row = row.clone();
                    row.run_id = "skid-import-consumer".into();
                    let observations = row.cpu_observations.as_mut().unwrap();
                    observations.binding.run_id = row.run_id.clone();
                    observations.binding.outer_attempt = row.attempt;
                    serde_json::to_value(row).unwrap()
                };
                let variants = if backend == "sabre" {
                    vec![
                        ("import-control", Expect::Retried),
                        ("import-pathless-pass", Expect::Retried),
                        ("import-pathless-recomputed-pass", Expect::Retried),
                        ("import-fallback-pass", Expect::Retried),
                        ("import-fallback-recomputed-pass", Expect::Retried),
                        ("import-misdigested-path-pass", Expect::Retried),
                        ("import-unselected-pass", Expect::Retried),
                        ("import-pathless-skid", Expect::Ended),
                        ("import-fallback-recomputed-skid", Expect::Ended),
                        ("import-misdigested-path-skid", Expect::Ended),
                        ("import-lone-skid", Expect::Unretried),
                    ]
                } else {
                    vec![
                        ("import-control", Expect::Retried),
                        ("import-undigested-pass", Expect::Retried),
                        ("import-misdigested-pass", Expect::Retried),
                        ("import-reportless-pass", Expect::Retried),
                        ("import-diverged-pass", Expect::Retried),
                        ("import-relaxed-pass", Expect::Retried),
                        ("import-misdigested-skid", Expect::Ended),
                        ("import-undigested-skid", Expect::Ended),
                        ("import-redeclared-skid", Expect::Ended),
                        ("import-vacuous-skid", Expect::Ended),
                        ("import-relaxed-skid", Expect::Ended),
                        ("import-plain-error-skid", Expect::Ended),
                        ("import-lone-skid", Expect::Unretried),
                    ]
                };
                for (variant, expect) in variants {
                    let mut imported = rows.clone();
                    for (number, row) in imported.iter_mut().enumerate() {
                        row.run_id = format!("skid-producer-{}", number + 1);
                        row.source_tree_dirty = false;
                        let observations = row
                            .cpu_observations
                            .as_mut()
                            .expect("an executed row carries CPU observations");
                        observations.binding.run_id = row.run_id.clone();
                        observations.binding.outer_attempt = 1;
                    }
                    // A `-skid` variant edits the skid row, every other one
                    // the PASS after it.
                    let row = &mut imported[usize::from(!variant.ends_with("-skid"))];
                    match variant {
                        "import-undigested-pass" | "import-undigested-skid" => {
                            row.attempts[0].verification_report_sha256 = None;
                        }
                        "import-misdigested-pass" | "import-misdigested-skid" => {
                            row.attempts[0].verification_report_sha256 = Some("0".repeat(64));
                        }
                        "import-reportless-pass" => {
                            row.attempts[0].verification_report = None;
                            row.attempts[0].verification_report_sha256 = None;
                        }
                        "import-diverged-pass" => {
                            row.attempts[0].verification_report_sha256 =
                                Some(sha256(&diverged_raw));
                            row.attempts[0].verification_report = Some(diverged_raw.clone());
                        }
                        "import-vacuous-skid" => {
                            row.attempts[0].verification_report_sha256 = Some(sha256(&vacuous_raw));
                            row.attempts[0].verification_report = Some(vacuous_raw.clone());
                        }
                        "import-redeclared-skid" => {
                            // A stdout declaration the row's own evidence
                            // meets, which is not the cell's: only the check
                            // that the row records its cell's declaration
                            // refuses it.
                            let redeclared = hermit_manifest_plan::runner::DeclaredStdout {
                                exact: None,
                                contains: Some(String::new()),
                            };
                            assert_ne!(row.declared_stdout.as_ref(), Some(&redeclared));
                            row.declared_stdout = Some(redeclared);
                            assert_eq!(
                                super::skid_overshoot_only_reports(row),
                                Some(2),
                                "{rows:#?}"
                            );
                        }
                        "import-plain-error-skid" => {
                            // An infrastructure ERROR with no verification
                            // report and no skid in its reason or stderr.
                            let reason = "producer infrastructure error";
                            row.reason = Some(reason.into());
                            row.attempts[0].reason = Some(reason.into());
                            row.attempts[0].verification_report = None;
                            row.attempts[0].verification_report_sha256 = None;
                            row.attempts[0].stderr = String::new();
                            assert_eq!(super::skid_overshoot_only_reports(row), None);
                        }
                        "import-relaxed-pass" | "import-relaxed-skid" => {
                            row.relaxations.push("--no-rcb-time: fixture reason".into());
                        }
                        "import-pathless-pass"
                        | "import-pathless-recomputed-pass"
                        | "import-pathless-skid" => {
                            row.attempts[0].sabre_path_evidence = None;
                            row.attempts[0].sabre_path_evidence_sha256 = None;
                            if variant == "import-pathless-recomputed-pass" {
                                // The summary of no records.
                                let path = row.execution_path.as_mut().unwrap();
                                path["complete"] = json!(false);
                                path["execution_count"] = json!(0);
                                path["guest_rpc_observed"] = json!(false);
                                path["eligible"] = json!(false);
                                path["executions"] = json!([]);
                            }
                        }
                        "import-fallback-pass"
                        | "import-fallback-recomputed-pass"
                        | "import-fallback-recomputed-skid" => {
                            row.attempts[0].sabre_path_evidence_sha256 =
                                Some(sha256(&fallback_evidence));
                            row.attempts[0].sabre_path_evidence = Some(fallback_evidence.clone());
                            if variant != "import-fallback-pass" {
                                // The summary of a run that used one ptrace
                                // fallback site.
                                let path = row.execution_path.as_mut().unwrap();
                                path["ptrace_fallback_sites"] = json!(1);
                                path["eligible"] = json!(false);
                                path["executions"][1]["ptrace_fallback_sites"] = json!(1);
                            }
                        }
                        "import-misdigested-path-pass" | "import-misdigested-path-skid" => {
                            row.attempts[0].sabre_path_evidence_sha256 = Some("0".repeat(64));
                        }
                        "import-unselected-pass" => {
                            // The attempt ran ptrace, and its CPU observations
                            // record the same command, so only the backend
                            // the row claims is wrong.
                            let selected = row.attempts[0].argv.clone();
                            let mut unselected = selected.clone();
                            let at = unselected
                                .iter()
                                .position(|arg| arg == "--backend")
                                .unwrap();
                            unselected[at + 1] = "ptrace".into();
                            row.attempts[0].argv = unselected.clone();
                            let observations = row.cpu_observations.as_mut().unwrap();
                            let mut rebound = 0;
                            for invocation in &mut observations.invocations {
                                if invocation.command.argv == selected {
                                    invocation.command.argv = unselected.clone();
                                    rebound += 1;
                                }
                            }
                            assert_eq!(rebound, 1, "{rows:#?}");
                        }
                        "import-control" | "import-lone-skid" => {}
                        _ => unreachable!("{variant}"),
                    }
                    if variant == "import-lone-skid" {
                        imported.truncate(1);
                    }
                    let (result, published, summary, context) = import(variant, &imported);
                    let stdout = String::from_utf8_lossy(&result.stdout);
                    let dropped = usize::from(matches!(expect, Expect::Ended));
                    assert!(
                        String::from_utf8_lossy(&result.stderr).contains(&format!(
                            "0 have no result; {dropped} producer retr(ies) this run would not have made were dropped"
                        )),
                        "{context}"
                    );
                    assert_eq!(
                        summary["imported"]["dropped_retries"],
                        json!(dropped),
                        "{context}"
                    );
                    let skid_line = format!(
                        "ERROR parity/skid (verify/{backend}) [SKID-RETRY: attempt 1 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)) and nothing else; retrying this cell only]"
                    );
                    let not_made = format!(
                        "SKID-RETRY NOT MADE parity/skid (verify/{backend}): attempt 1 of at most 2 is a typed HERMIT_SKID_OVERSHOOT infrastructure error (2 report(s)), but it is an imported attempt and its producer recorded no later attempt of the cell; the cell's selected outcome is ERROR (attempt 1)\n"
                    );
                    assert_eq!(
                        stdout.contains("SKID-RETRY NOT MADE"),
                        matches!(expect, Expect::Unretried),
                        "{context}"
                    );
                    match expect {
                        Expect::Retried => {
                            // Both producer rows, each as its producer wrote
                            // it apart from this run's binding, and the
                            // retry counted as an executed run counts it.
                            assert_eq!(
                                published.iter().map(rebound).collect::<Vec<_>>(),
                                imported.iter().map(rebound).collect::<Vec<_>>(),
                                "{context}"
                            );
                            assert_eq!(
                                published
                                    .iter()
                                    .map(|row| (
                                        row.attempt,
                                        row.outcome.as_str(),
                                        row.error_kind.as_deref()
                                    ))
                                    .collect::<Vec<_>>(),
                                vec![(1, "ERROR", Some("infrastructure")), (2, "PASS", None)],
                                "{context}"
                            );
                            assert_eq!(result.status.code(), Some(0), "{context}");
                            assert_eq!(summary["passed"], json!(1), "{context}");
                            assert_eq!(summary["errors"], json!(0), "{context}");
                            assert_eq!(summary["skid_retries"], json!(1), "{context}");
                            assert_eq!(
                                summary["skid_retry_cells"],
                                json!([{
                                    "test": "parity/skid", "mode": "verify", "backend": backend,
                                    "attempt": 1, "overshoot_reports": 2, "final_outcome": "PASS",
                                }]),
                                "{context}"
                            );
                            assert!(stdout.contains("; SKID-RETRY count 1\n"), "{context}");
                            assert!(stdout.contains(&skid_line), "{context}");
                            assert!(
                                stdout.contains(&format!(
                                    "test-harness: SKID-RETRY parity/skid (verify/{backend}): attempt 1 recorded 2 HERMIT_SKID_OVERSHOOT report(s) and was retried; final outcome PASS"
                                )),
                                "{context}"
                            );
                            if variant != "import-control" {
                                // The same PASS as its producer's only
                                // attempt ends the same way.
                                let mut alone = imported[1].clone();
                                alone.attempt = 1;
                                let (lone, lone_published, lone_summary, lone_context) =
                                    import(&format!("{variant}-alone"), &[alone]);
                                assert_eq!(lone.status.code(), Some(0), "{lone_context}");
                                assert_eq!(
                                    lone_published
                                        .iter()
                                        .map(|row| (
                                            row.outcome.as_str(),
                                            row.error_kind.as_deref()
                                        ))
                                        .collect::<Vec<_>>(),
                                    vec![("PASS", None)],
                                    "{lone_context}"
                                );
                                assert_eq!(lone_summary["passed"], json!(1), "{lone_context}");
                            }
                        }
                        Expect::Ended | Expect::Unretried => {
                            // The skid row itself, as its producer wrote it,
                            // and nothing after it; its ERROR fails the run.
                            assert_eq!(
                                published.iter().map(rebound).collect::<Vec<_>>(),
                                vec![rebound(&imported[0])],
                                "{context}"
                            );
                            let row = &published[0];
                            assert_eq!(
                                (row.attempt, row.outcome.as_str(), row.error_kind.as_deref()),
                                (1, "ERROR", Some("infrastructure")),
                                "{context}"
                            );
                            assert_eq!(result.status.code(), Some(1), "{context}");
                            assert_eq!(summary["errors"], json!(1), "{context}");
                            assert_eq!(summary["skid_retries"], json!(0), "{context}");
                            assert_eq!(summary["skid_retry_cells"], json!([]), "{context}");
                            assert!(stdout.contains("; SKID-RETRY count 0\n"), "{context}");
                            assert!(!stdout.contains("SKID-RETRY:"), "{context}");
                            assert_eq!(
                                stdout.contains(&not_made),
                                matches!(expect, Expect::Unretried),
                                "{context}"
                            );
                        }
                    }
                }
            }
            fs::remove_dir_all(&fixture).unwrap();
        }
    }

    /// The manifests of the import-mode fixtures: one test whose CI cells are
    /// verify@ptrace, verify@kvm and custom@kvm.
    fn write_import_fixture(path: &std::path::Path) {
        use serde_json::json;

        let manifests_dir = path.join("tests/e2e/manifests");
        fs::create_dir_all(&manifests_dir).unwrap();
        fs::write(
            manifests_dir.join("defaults.yaml"),
            "schema: 3\ntimeout_seconds: 10\ncpu_timeout_seconds: 5\n",
        )
        .unwrap();
        let disabled = json!({"ci": false, "backends_enabled": [], "backends_disabled": {
            "ptrace": "Not selected by this control", "dbt": "Not selected by this control",
            "kvm": "Not selected by this control", "sabre": "Not selected by this control",
            "liteinst": "Not selected by this control",
            "in-guest-trap": "Not selected by this control"
        }});
        fs::write(manifests_dir.join("imported.yaml"), serde_json::to_vec(&json!({
            "schema": 3, "bucket": "imported", "test": [{
                "id": "imported/control", "description": "Import mode control",
                "lane": "portable", "occasional": false, "direct": ["/bin/true"],
                "observation": {"status": true, "stdout": true, "stderr": true},
                "modes": {
                    "verify": {"ci": true, "backends_enabled": ["ptrace", "kvm"],
                        "backends_disabled": {"dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected", "in-guest-trap": "Not selected"}},
                    "naked": {"ci": false, "backends_enabled": [],
                        "backends_disabled": {"native": "Not selected by this CI control"}},
                    "chaos": disabled, "replay": disabled,
                    "custom": {"ci": true, "backends_enabled": ["kvm"],
                        "backends_disabled": {"ptrace": "Not selected", "dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected", "in-guest-trap": "Not selected"},
                        "assert": {"runs": 1}}
                }
            }]
        })).unwrap()).unwrap();
    }

    /// A row as `ci/buck-e2e/ingest.py` leaves it: the producer execution was
    /// its own `--no-retry` harness run (run id `run_id`, outer attempt 1), and
    /// the ingest numbered it `attempt` in Tpx's execution order.
    fn import_producer_row(
        producer: &hermit_manifest_plan::runner::RunContext,
        cell: &hermit_manifest_plan::runner::SelectedCell,
        run_id: String,
        attempt: u64,
        outcome: &str,
    ) -> CellResult {
        use hermit_manifest_plan::runner::ObservedResult;
        use hermit_manifest_plan::runner::host_inapplicable_result;
        use hermit_manifest_plan::runner::infrastructure_error_result;

        let mut context = producer.with_attempt(1);
        context.run_id = run_id;
        let mut row = infrastructure_error_result(&context, cell, String::new());
        match outcome {
            // A producer never writes one: it reports host inapplicability
            // only in summary.json.
            "HOST-INAPPLICABLE" => {
                row = host_inapplicable_result(&context, cell, "producer: no kvm".into());
            }
            "PASS" => {
                row.outcome = "PASS".into();
                row.error_kind = None;
                row.reason = None;
                row.result = Some(ObservedResult::Pass);
                row.failure_class = None;
            }
            "FAIL" => {
                row.outcome = "FAIL".into();
                row.error_kind = None;
                row.reason = None;
                row.result = Some(ObservedResult::DeterminismFailure);
                row.failure_class = Some(FailureClass::ProductFailure);
            }
            // An infrastructure ERROR, which this run's policy never retries.
            "ERROR" => row.reason = Some("producer infrastructure error".into()),
            _ => unreachable!(),
        }
        row.require_current_classification().unwrap();
        row.require_cpu_observations().unwrap();
        row.attempt = attempt;
        row
    }

    fn import_summary_cell(mode: &str, backend: &str) -> serde_json::Value {
        serde_json::json!({"test": "imported/control", "mode": mode, "backend": backend})
    }

    /// The evidence [`host_capabilities_hermit`] reports for its
    /// `exact_branch_counter` verdict.
    const PLANTED_COUNTER_EVIDENCE: &str = "planted: Reverie performance-counter validation failed";

    /// A Hermit in `dir` that answers only `host-capabilities --json`, with a
    /// complete report whose `exact_branch_counter` verdict is `present`.
    fn host_capabilities_hermit(dir: &std::path::Path, present: bool) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        use hermit_manifest_plan::host_capability::HostCapabilitiesReport;

        let verdict = |present| HostCapabilityVerdict {
            present,
            evidence: PLANTED_COUNTER_EVIDENCE.into(),
        };
        let report = HostCapabilitiesReport {
            schema: HostCapabilitiesReport::SCHEMA,
            host_capabilities: HostCapability::ALL
                .into_iter()
                .map(|capability| (capability, verdict(true)))
                .collect(),
            exact_branch_counter: verdict(present),
        };
        let hermit = dir.join("hermit");
        fs::write(
            &hermit,
            format!(
                "#!/bin/sh\ntest \"$*\" = 'host-capabilities --json' || exit 2\ncat <<'EOF'\n{}\nEOF\n",
                serde_json::to_string(&report).unwrap()
            ),
        )
        .unwrap();
        fs::set_permissions(&hermit, fs::Permissions::from_mode(0o755)).unwrap();
        hermit
    }

    /// The `evidence_complete_executions` entry of the execution that wrote
    /// `row`.
    fn import_execution(row: &CellResult) -> serde_json::Value {
        serde_json::json!({
            "test": row.test, "mode": row.mode, "backend": row.backend, "run_id": row.run_id
        })
    }

    /// `E2E_IMPORT_RESULTS` executes no cell: it re-publishes the rows a Buck
    /// run left under `<root>/<lane>/manifest_<category>/`. A complete import
    /// passes with every attempt rebound to this run. Each way an imported
    /// cell could end better than an executed run would have makes it end
    /// worse instead: a selected cell with no row (the executed-equals-plan
    /// gate); rows of another commit, a dirty tree, another stamped binary,
    /// other test source, other timeouts or CPU evidence bound elsewhere; a
    /// history no producer writes (an attempt after a PASS, a repeated or
    /// out-of-order attempt, a HOST-INAPPLICABLE row); a producer retry this
    /// run would not have made, after an infrastructure ERROR or under
    /// `--no-retry`; a host-inapplicable claim this machine does not confirm,
    /// or one alongside rows; and a PASS whose own execution has no
    /// evidence-complete record. A claim that a strict ptrace cell cannot run
    /// stands when this machine's Hermit reports an inexact branch counter,
    /// and not when it reports an exact one or cannot be asked.
    #[test]
    fn import_mode_republishes_rows_and_a_missing_cell_is_an_error() {
        use std::path::PathBuf;
        use std::process::Command;
        use std::process::ExitCode;

        use hermit_manifest_plan::imported_results::bucket_dir;
        use hermit_manifest_plan::runner::RunContext;
        use serde_json::json;

        const CHILD: &str = "HERMIT_IMPORT_MODE_FIXTURE";
        const NO_RETRY: &str = "HERMIT_IMPORT_MODE_FIXTURE_NO_RETRY";
        const TEST: &str = "tests::import_mode_republishes_rows_and_a_missing_cell_is_an_error";
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        const STALE: &str = "fedcba9876543210fedcba9876543210fedcba98";
        if let Some(fixture) = std::env::var_os(CHILD) {
            // The fixture is the `git archive` --source-sha names: Git tracks
            // none of its files.
            let fixture = PathBuf::from(fixture);
            let mut values = vec![
                "--category".into(),
                "imported".into(),
                "--ci-only".into(),
                "--prebuilt".into(),
                "--repo-root".into(),
                fixture.display().to_string(),
                "--source-sha".into(),
                SHA.into(),
                "--results".into(),
                fixture.join("out/results.jsonl").display().to_string(),
                "--junit".into(),
                fixture.join("out/junit.xml").display().to_string(),
            ];
            if std::env::var_os(NO_RETRY).is_some() {
                values.push("--no-retry".into());
            }
            let args = parse(values.into_iter());
            validate_args("run", &args);
            let manifests = ManifestSet::load(&fixture).unwrap();
            let code = super::run(&fixture, &manifests, &args);
            std::process::exit(if code == ExitCode::SUCCESS { 0 } else { 1 });
        }

        for scenario in [
            "complete",
            "missing",
            "stale",
            "dirty",
            "stamped",
            "test-source",
            "timeouts",
            "binding",
            "stale-first-row",
            "untrusted-retry",
            "no-retry",
            "pass-then-fail",
            "pass-then-error",
            "pass-then-pass",
            "duplicate-attempt",
            "file-order",
            "file-order-pass-first",
            "rowless-first-execution",
            "host-inapplicable-then-fail",
            "host-inapplicable-row",
            "unconfirmed",
            "conflict",
            "counter-confirmed",
            "counter-unconfirmed",
            "counter-unprobed",
            "evidence",
            "evidence-elsewhere",
        ] {
            let counter_claim = scenario.starts_with("counter-");
            let fixture = std::env::temp_dir().join(format!(
                "hermit-harness-import-{}-{:?}-{scenario}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::create_dir(&fixture).unwrap();
            let path = fixture.as_path();
            write_import_fixture(path);

            // The rows a Buck run would have left: each cell ran in its own
            // harness process with its own run id; custom@kvm failed its first
            // execution and passed its Tpx retry. Every execution recorded
            // complete evidence unless the scenario withholds it.
            let manifests = ManifestSet::load(path).unwrap();
            let selection = parse(
                ["--category", "imported", "--ci-only"]
                    .into_iter()
                    .map(String::from),
            );
            let cells = run_cells(&manifests, &selection).unwrap();
            assert_eq!(cells.len(), 3, "verify@ptrace, verify@kvm, custom@kvm");
            let producer = RunContext::for_import(path.to_path_buf(), Some(SHA)).unwrap();
            let mut rows = Vec::new();
            let mut evidence = Vec::new();
            for (index, cell) in cells.iter().enumerate() {
                let ptrace = cell.id.backend.as_deref() == Some("ptrace");
                let verify_kvm = cell.id.mode == "verify" && !ptrace;
                if verify_kvm && matches!(scenario, "missing" | "unconfirmed") {
                    continue;
                }
                if ptrace && counter_claim {
                    continue;
                }
                // (attempt, outcome), in file order.
                let attempts: &[(u64, &str)] = match (cell.id.mode.as_str(), scenario) {
                    ("custom", "untrusted-retry") => &[(1, "ERROR"), (2, "PASS")],
                    ("custom", "pass-then-fail") => &[(1, "PASS"), (2, "FAIL")],
                    ("custom", "pass-then-error") => &[(1, "PASS"), (2, "ERROR")],
                    ("custom", "pass-then-pass") => &[(1, "PASS"), (2, "PASS")],
                    ("custom", "duplicate-attempt") => &[(1, "PASS"), (1, "FAIL")],
                    ("custom", "file-order") => &[(2, "FAIL"), (1, "PASS")],
                    // Sorted by attempt, this history would end in a PASS.
                    ("custom", "file-order-pass-first") => &[(2, "PASS"), (1, "FAIL")],
                    // The first execution died before writing a row, and the
                    // ingest still numbered it attempt 1.
                    ("custom", "rowless-first-execution") => &[(2, "PASS")],
                    ("custom", "host-inapplicable-then-fail") => {
                        &[(1, "HOST-INAPPLICABLE"), (2, "FAIL")]
                    }
                    ("custom", _) => &[(1, "FAIL"), (2, "PASS")],
                    (_, "host-inapplicable-row") if verify_kvm => &[(1, "HOST-INAPPLICABLE")],
                    _ => &[(1, "PASS")],
                };
                for (number, &(attempt, outcome)) in attempts.iter().enumerate() {
                    let mut producer = producer.clone();
                    let stale = match scenario {
                        "stale" => ptrace,
                        // Only custom@kvm's failed first execution: its
                        // passing retry is current.
                        "stale-first-row" => cell.id.mode == "custom" && number == 0,
                        _ => false,
                    };
                    if stale {
                        producer.source_sha = STALE.into();
                    }
                    let mut row = import_producer_row(
                        &producer,
                        cell,
                        format!("buck-producer-{index}-{number}"),
                        attempt,
                        outcome,
                    );
                    // A binary stamped with this commit's short sha is this
                    // commit's binary.
                    if verify_kvm {
                        row.binary_build_sha = Some(SHA[..12].into());
                    }
                    if ptrace {
                        match scenario {
                            "dirty" => row.source_tree_dirty = true,
                            "stamped" => row.binary_build_sha = Some(STALE[..12].into()),
                            "test-source" => row.test_sha256 = "0".repeat(64),
                            "timeouts" => row.execution_wall_timeout_seconds = Some(999),
                            "binding" => {
                                row.cpu_observations.as_mut().unwrap().binding.run_id =
                                    "another-producer-run".into();
                            }
                            _ => {}
                        }
                    }
                    let withheld = match scenario {
                        "evidence" => verify_kvm,
                        // Only custom@kvm's failed first execution has it.
                        "evidence-elsewhere" => cell.id.mode == "custom" && outcome == "PASS",
                        _ => false,
                    };
                    if !withheld {
                        evidence.push(import_execution(&row));
                    }
                    rows.push(serde_json::to_string(&row).unwrap());
                }
            }
            let import = path.join("import");
            let bucket = bucket_dir(&import, "portable", "imported");
            fs::create_dir_all(&bucket).unwrap();
            fs::write(bucket.join("results.jsonl"), rows.join("\n") + "\n").unwrap();
            let host_inapplicable_cells = match scenario {
                "unconfirmed" => vec![("verify", "kvm", "kvm")],
                "conflict" => vec![("verify", "ptrace", "kvm")],
                // The producer reran verify@ptrace on a host whose retired-
                // branch counter is inexact, where the binary refuses --strict.
                _ if counter_claim => vec![("verify", "ptrace", "exact-branch-counter")],
                _ => vec![],
            }
            .into_iter()
            .map(|(mode, backend, capability)| {
                let mut cell = import_summary_cell(mode, backend);
                cell["reason"] = format!("producer: this machine lacks {capability}").into();
                cell
            })
            .collect::<Vec<_>>();
            // No cell launches Hermit: the import asks it only for this
            // machine's exact_branch_counter verdict, which the counter
            // scenarios plant and an absent binary cannot give.
            let hermit = match scenario {
                "counter-confirmed" => host_capabilities_hermit(path, false),
                "counter-unconfirmed" => host_capabilities_hermit(path, true),
                _ => path.join("absent-hermit"),
            };
            fs::write(
                bucket.join("summary.json"),
                serde_json::to_vec(&json!({
                    "host_inapplicable_cells": host_inapplicable_cells,
                    "evidence_complete_executions": evidence,
                }))
                .unwrap(),
            )
            .unwrap();

            let mut command = Command::new("timeout");
            command
                .args(["--kill-after=2s", "30s"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env(CHILD, path)
                .env("E2E_IMPORT_RESULTS", &import)
                // This machine can run every fixture cell, whatever the host.
                .env("HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT", "kvm")
                .env("HERMIT_BIN", &hermit)
                .env("E2E_RESULT_ROOT", path.join("artifacts"))
                .env("E2E_BUILD_ROOT", path.join("build"))
                .env("E2E_RUN_ID", "import-consumer")
                .env("E2E_MACHINE_SHORTNAME", "import-control")
                .env("E2E_KERNEL_VERSION", "import-control")
                .env("DAGRUN_TEST_COUNTS_PATH", path.join("counts.json"));
            if scenario == "no-retry" {
                command.env(NO_RETRY, "1");
            }
            let result = command.output().unwrap();
            let context = format!(
                "{scenario}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            let confirmed = scenario == "counter-confirmed";
            let succeeds = scenario == "complete" || confirmed;
            assert_eq!(
                result.status.code(),
                Some(if succeeds { 0 } else { 1 }),
                "{context}"
            );
            let published = fs::read_to_string(path.join("out/results.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
                .collect::<Vec<_>>();
            let summary: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("out/summary.json")).unwrap()).unwrap();
            assert_eq!(summary["cells"], 3, "{context}");
            for row in &published {
                assert_eq!(row.run_id, "import-consumer", "{context}");
                row.require_cpu_observations().unwrap();
            }
            let errors = published
                .iter()
                .filter(|row| row.outcome == "ERROR")
                .map(|row| {
                    (
                        row.mode.as_str(),
                        row.backend.as_deref(),
                        row.error_kind.as_deref(),
                    )
                })
                .collect::<Vec<_>>();
            let custom = published
                .iter()
                .filter(|row| row.mode == "custom")
                .map(|row| (row.attempt, row.outcome.as_str()))
                .collect::<Vec<_>>();
            let expected_error = match scenario {
                "complete" => None,
                "missing" => Some(("verify", Some("kvm"), Some("import-missing"))),
                "stale" | "dirty" | "stamped" | "test-source" | "timeouts" | "binding" => {
                    Some(("verify", Some("ptrace"), Some("import-stale")))
                }
                "stale-first-row" => Some(("custom", Some("kvm"), Some("import-stale"))),
                "untrusted-retry" => Some(("custom", Some("kvm"), Some("infrastructure"))),
                // Its FAIL is the verdict, and a FAIL is not an ERROR.
                "no-retry" => None,
                "pass-then-fail"
                | "pass-then-error"
                | "pass-then-pass"
                | "duplicate-attempt"
                | "file-order"
                | "file-order-pass-first"
                | "rowless-first-execution"
                | "host-inapplicable-then-fail" => {
                    Some(("custom", Some("kvm"), Some("import-history")))
                }
                "host-inapplicable-row" => Some(("verify", Some("kvm"), Some("import-history"))),
                "unconfirmed" => Some((
                    "verify",
                    Some("kvm"),
                    Some("import-host-inapplicable-unconfirmed"),
                )),
                "conflict" => Some(("verify", Some("ptrace"), Some("import-conflict"))),
                "counter-confirmed" => None,
                "counter-unconfirmed" | "counter-unprobed" => Some((
                    "verify",
                    Some("ptrace"),
                    Some("import-host-inapplicable-unconfirmed"),
                )),
                "evidence" => Some(("verify", Some("kvm"), Some("import-evidence-incomplete"))),
                "evidence-elsewhere" => {
                    Some(("custom", Some("kvm"), Some("import-evidence-incomplete")))
                }
                _ => unreachable!(),
            };
            assert_eq!(errors, Vec::from_iter(expected_error), "{context}");
            assert_eq!(
                summary["errors"],
                u64::from(expected_error.is_some()),
                "{context}"
            );
            // Each history no producer writes is refused for its own reason.
            let history_reason = match scenario {
                "pass-then-fail" | "pass-then-error" | "pass-then-pass" => {
                    Some("attempt 2 follows terminal outcome PASS")
                }
                "duplicate-attempt" => {
                    Some("attempt 1 does not follow the preceding attempts; expected 2")
                }
                "file-order" | "file-order-pass-first" | "rowless-first-execution" => {
                    Some("attempt 2 does not follow the preceding attempts; expected 1")
                }
                "host-inapplicable-then-fail" | "host-inapplicable-row" => {
                    Some("imported attempt 1 is a HOST-INAPPLICABLE row")
                }
                _ => None,
            };
            if let Some(expected) = history_reason {
                let reason = published
                    .iter()
                    .find(|row| row.error_kind.as_deref() == Some("import-history"))
                    .and_then(|row| row.reason.as_deref())
                    .unwrap_or_default();
                assert!(reason.contains(expected), "{reason}\n{context}");
            }
            // Every row is checked, not only the one that decides the cell.
            if scenario == "stale-first-row" {
                let reason = published
                    .iter()
                    .find(|row| row.error_kind.as_deref() == Some("import-stale"))
                    .and_then(|row| row.reason.as_deref())
                    .unwrap_or_default();
                let expected = format!("imported row was built from {STALE}, this run is {SHA}");
                assert!(reason.contains(&expected), "{reason}\n{context}");
            }
            assert_eq!(
                summary["imported"]["missing_cells"],
                u64::from(scenario == "missing"),
                "{context}"
            );
            assert_eq!(
                summary["imported"]["dropped_retries"],
                u64::from(matches!(scenario, "untrusted-retry" | "no-retry")),
                "{context}"
            );
            match scenario {
                // The producer's retry after an infrastructure ERROR is not
                // one this run would have made: the ERROR is the verdict.
                // Under --no-retry, no retry is: the first FAIL is.
                "untrusted-retry" => assert_eq!(custom, vec![(1, "ERROR")], "{context}"),
                "no-retry" => {
                    assert_eq!(custom, vec![(1, "FAIL")], "{context}");
                    assert_eq!(summary["failed"], 1, "{context}");
                }
                // One import ERROR replaces custom@kvm's rows.
                "pass-then-fail"
                | "pass-then-error"
                | "pass-then-pass"
                | "duplicate-attempt"
                | "file-order"
                | "file-order-pass-first"
                | "rowless-first-execution"
                | "host-inapplicable-then-fail"
                | "stale-first-row"
                | "evidence-elsewhere" => assert_eq!(custom, vec![(1, "ERROR")], "{context}"),
                // Both custom@kvm attempts are published, in order.
                _ => assert_eq!(custom, vec![(1, "FAIL"), (2, "PASS")], "{context}"),
            }
            // A confirmed claim publishes no row, since the cell did not run:
            // the summary names it with this machine's reason, which quotes
            // the evidence of this machine's Hermit, not the producer's words.
            let did_not_run = summary["host_inapplicable_cells"]
                .as_array()
                .unwrap()
                .iter()
                .map(|claim| {
                    (
                        claim["mode"].as_str().unwrap(),
                        claim["backend"].as_str(),
                        claim["reason"].as_str().unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            if confirmed {
                let [(mode, backend, reason)] = did_not_run[..] else {
                    panic!("one cell did not run: {did_not_run:?}\n{context}");
                };
                assert_eq!((mode, backend), ("verify", Some("ptrace")), "{context}");
                assert!(
                    reason.contains(&format!(
                        "lacks exact-branch-counter ({PLANTED_COUNTER_EVIDENCE})"
                    )) && !reason.contains("producer:"),
                    "{reason}\n{context}"
                );
                assert!(
                    published
                        .iter()
                        .all(|row| row.backend.as_deref() != Some("ptrace")),
                    "{context}"
                );
            } else {
                assert_eq!(did_not_run, Vec::new(), "{context}");
            }
            let host_inapplicable = u64::from(confirmed);
            assert_eq!(summary["host_inapplicable"], host_inapplicable, "{context}");
            // JUnit and the dagrun counts report the same final outcomes, and
            // the counts each executed cell's published attempts.
            let failed = u64::from(scenario == "no-retry");
            assert_eq!(summary["failed"], failed, "{context}");
            let junit = fs::read_to_string(path.join("out/junit.xml")).unwrap();
            let header = format!(
                "tests=\"3\" failures=\"{failed}\" errors=\"{}\" skipped=\"{host_inapplicable}\"",
                u64::from(expected_error.is_some())
            );
            assert!(junit.contains(&header), "{junit}\n{context}");
            let counts: serde_json::Value =
                serde_json::from_slice(&fs::read(path.join("counts.json")).unwrap()).unwrap();
            let by_id = |results: &serde_json::Value| {
                let mut results = results.as_array().unwrap().clone();
                results.sort_by_key(|result| result["id"].as_str().unwrap().to_string());
                results
            };
            let expected_counts = cells
                .iter()
                // A cell that did not run is no executed test.
                .filter(|cell| {
                    !did_not_run.iter().any(|&(mode, backend, _)| {
                        (mode, backend) == (cell.id.mode.as_str(), cell.id.backend.as_deref())
                    })
                })
                .map(|cell| {
                    let history = published
                        .iter()
                        .filter(|row| row.mode == cell.id.mode && row.backend == cell.id.backend)
                        .collect::<Vec<_>>();
                    json!({
                        "id": format!(
                            "imported/control [{}/{}]",
                            cell.id.backend.as_deref().unwrap_or("native"),
                            cell.id.mode
                        ),
                        "result": if history.last().unwrap().outcome == "PASS" { "pass" } else { "fail" },
                        "attempts": history.len(),
                    })
                })
                .collect::<serde_json::Value>();
            assert_eq!(counts["schema"], 2, "{context}");
            assert_eq!(counts["executed_tests"], 3 - host_inapplicable, "{context}");
            assert_eq!(
                by_id(&counts["results"]),
                by_id(&expected_counts),
                "{context}"
            );
            if scenario == "complete" {
                assert_eq!(published.len(), 4, "{context}");
                assert_eq!(summary["passed"], 3, "{context}");
                assert_eq!(
                    by_id(&counts["results"]),
                    by_id(&json!([
                        {"id": "imported/control [kvm/custom]", "result": "pass", "attempts": 2},
                        {"id": "imported/control [kvm/verify]", "result": "pass", "attempts": 1},
                        {"id": "imported/control [ptrace/verify]", "result": "pass", "attempts": 1}
                    ])),
                    "{context}"
                );
                assert_eq!(
                    summary["imported"]["source_run_ids"]
                        .as_array()
                        .unwrap()
                        .len(),
                    4,
                    "{context}"
                );
            }
            fs::remove_dir_all(path).unwrap();
        }
    }

    /// A producer's host-inapplicable claim stands only when this machine
    /// lacks a capability the cell needs, and the published row carries this
    /// machine's reason, not the producer's. No environment can make a host
    /// capability absent, so this drives `imported_results::load` with the
    /// policy the harness would build on such a machine.
    /// A Buck remote-execution worker decides its own strictness level: one
    /// whose branch counter miscounts runs a ptrace verify cell at the relaxed
    /// level, and its row says so. The import keeps that row as the producer
    /// wrote it, a PASS at the relaxed level, rather than judging it by this
    /// machine's level (https://github.com/rrnewton/hermit/issues/4291).
    #[test]
    fn import_mode_keeps_a_relaxed_row_from_a_worker_whose_counter_miscounts() {
        use hermit_manifest_plan::imported_results;
        use hermit_manifest_plan::imported_results::bucket_dir;
        use hermit_manifest_plan::runner::HostLevel;
        use hermit_manifest_plan::runner::RELAXED_LEVEL_ARGS;
        use hermit_manifest_plan::runner::RunContext;
        use hermit_manifest_plan::runner::StrictnessLevel;
        use hermit_manifest_plan::runner::host_strictness;
        use serde_json::json;

        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-import-relaxed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&fixture).unwrap();
        write_import_fixture(&fixture);
        let manifests = ManifestSet::load(&fixture).unwrap();
        let selection = parse(
            ["--category", "imported", "--backend", "ptrace", "--ci-only"]
                .into_iter()
                .map(String::from),
        );
        let cells = run_cells(&manifests, &selection).unwrap();
        let context = RunContext::for_import(fixture.clone(), Some(SHA)).unwrap();

        // The worker: no CPUID faulting, and a counter that fails Reverie's
        // validation, as the AMD EPYC 9D64 of the issue does.
        let mut worker = context.clone();
        worker.exact_branch_counter =
            Some(hermit_manifest_plan::host_capability::CapabilityVerdict {
                present: false,
                evidence:
                    "Reverie performance-counter validation failed (AmdSpecLockMapShouldBeDisabled)"
                        .into(),
            });
        let worker_level = host_strictness(
            &worker.host_capabilities,
            worker.exact_branch_counter.as_ref(),
            None,
        )
        .unwrap();
        assert_eq!(worker_level.level, StrictnessLevel::Relaxed);
        let import = fixture.join("import");
        let bucket = bucket_dir(&import, "portable", "imported");
        fs::create_dir_all(&bucket).unwrap();
        let (rows, evidence): (Vec<_>, Vec<_>) = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let mut ran = cell.clone();
                ran.host_level = HostLevel::clone(&worker_level);
                let row =
                    import_producer_row(&worker, &ran, format!("buck-worker-{index}"), 1, "PASS");
                (serde_json::to_string(&row).unwrap(), import_execution(&row))
            })
            .unzip();
        assert!(!rows.is_empty());
        fs::write(bucket.join("results.jsonl"), rows.join("\n") + "\n").unwrap();
        fs::write(
            bucket.join("summary.json"),
            serde_json::to_vec(&json!({"evidence_complete_executions": evidence})).unwrap(),
        )
        .unwrap();

        // This machine runs every cell.
        let earns_retry = |_: &SelectedCell, _: &CellResult| true;
        let host_inapplicable = |_: &SelectedCell| None;
        let policy = imported_results::ImportPolicy {
            earns_retry: &earns_retry,
            host_inapplicable: &host_inapplicable,
        };
        let imported = imported_results::load(
            &import,
            &cells,
            &context,
            &fixture.join("out/results.jsonl"),
            &policy,
        )
        .unwrap();
        for (cell, imported) in cells.iter().zip(&imported.cells) {
            let [row] = imported.rows.as_slice() else {
                panic!("one row per cell: {:?}", imported.rows);
            };
            assert_eq!(row.outcome, "PASS", "{:?}", cell.id);
            let strictness = row.strictness.expect("the worker records its level");
            if cell.id.mode == "verify" {
                assert_eq!(strictness.level, StrictnessLevel::Relaxed, "{:?}", cell.id);
                assert!(!strictness.virtualize_cpuid);
                for flag in RELAXED_LEVEL_ARGS {
                    assert!(
                        row.relaxations
                            .iter()
                            .any(|relaxation| relaxation.starts_with(&format!("{flag}: "))),
                        "{flag}: {:?}",
                        row.relaxations
                    );
                }
            }
            assert_eq!(
                row.exact_branch_counter
                    .as_ref()
                    .map(|verdict| verdict.present),
                Some(false)
            );
        }
        assert_eq!(imported.missing, 0);
        fs::remove_dir_all(&fixture).unwrap();
    }

    #[test]
    fn import_mode_host_inapplicable_claim_needs_this_machines_confirmation() {
        use hermit_manifest_plan::imported_results;
        use hermit_manifest_plan::imported_results::bucket_dir;
        use hermit_manifest_plan::runner::RunContext;
        use serde_json::json;

        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-import-claim-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&fixture).unwrap();
        write_import_fixture(&fixture);
        let manifests = ManifestSet::load(&fixture).unwrap();
        let selection = parse(
            ["--category", "imported", "--ci-only"]
                .into_iter()
                .map(String::from),
        );
        let cells = run_cells(&manifests, &selection).unwrap();
        let context = RunContext::for_import(fixture.clone(), Some(SHA)).unwrap();
        // The import's parity post-pass runs `hermit log-diff` with the Hermit the Buck cells
        // ran, never the target/debug/hermit a Buck run does not build.
        if std::env::var_os("HERMIT_BIN").is_none() {
            assert_eq!(
                context.hermit_bin,
                fixture.join("ci/buck-e2e/staged/hermit")
            );
        }
        let import = fixture.join("import");
        let bucket = bucket_dir(&import, "portable", "imported");
        fs::create_dir_all(&bucket).unwrap();
        let (rows, evidence): (Vec<_>, Vec<_>) = cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.id.backend.as_deref() == Some("ptrace"))
            .map(|(index, cell)| {
                let row = import_producer_row(
                    &context,
                    cell,
                    format!("buck-producer-{index}"),
                    1,
                    "PASS",
                );
                (serde_json::to_string(&row).unwrap(), import_execution(&row))
            })
            .unzip();
        fs::write(bucket.join("results.jsonl"), rows.join("\n") + "\n").unwrap();
        let claims = [("verify", "kvm"), ("custom", "kvm")]
            .into_iter()
            .map(|(mode, backend)| {
                let mut cell = import_summary_cell(mode, backend);
                cell["reason"] = "producer's reason".into();
                cell
            })
            .collect::<Vec<_>>();
        fs::write(
            bucket.join("summary.json"),
            serde_json::to_vec(&json!({
                "host_inapplicable_cells": claims,
                "evidence_complete_executions": evidence,
            }))
            .unwrap(),
        )
        .unwrap();

        // This machine lacks KVM for verify@kvm only.
        let earns_retry = |_: &SelectedCell, _: &CellResult| true;
        let host_inapplicable = |cell: &SelectedCell| {
            (cell.id.mode == "verify" && cell.id.backend.as_deref() == Some("kvm"))
                .then(|| "this machine lacks kvm".to_string())
        };
        let policy = imported_results::ImportPolicy {
            earns_retry: &earns_retry,
            host_inapplicable: &host_inapplicable,
        };
        let imported = imported_results::load(
            &import,
            &cells,
            &context,
            &fixture.join("out/results.jsonl"),
            &policy,
        )
        .unwrap();
        let mut verdicts = cells
            .iter()
            .zip(&imported.cells)
            .map(|(cell, imported)| {
                let [row] = imported.rows.as_slice() else {
                    panic!("one row per cell: {:?}", imported.rows);
                };
                (
                    format!("{}@{}", cell.id.mode, cell.id.backend.as_deref().unwrap()),
                    (
                        row.outcome.clone(),
                        row.error_kind.clone(),
                        row.reason.clone().unwrap_or_default(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let (outcome, error_kind, reason) = verdicts.remove("custom@kvm").unwrap();
        assert_eq!(
            (outcome.as_str(), error_kind.as_deref()),
            ("ERROR", Some("import-host-inapplicable-unconfirmed"))
        );
        assert!(reason.contains("producer's reason"), "{reason}");
        assert_eq!(
            verdicts,
            BTreeMap::from([
                ("verify@ptrace".into(), ("PASS".into(), None, String::new())),
                (
                    "verify@kvm".into(),
                    (
                        "HOST-INAPPLICABLE".into(),
                        None,
                        "this machine lacks kvm".into()
                    )
                ),
            ])
        );
        assert_eq!(imported.missing, 0);
        fs::remove_dir_all(&fixture).unwrap();
    }

    /// An imported row records whether this checkout is dirty, as an executed
    /// row does: clean producer rows do not make a dirty checkout's run look
    /// clean. (`run` in the fixture above names a `--source-sha` snapshot,
    /// which is clean by construction and is refused where Git tracks the
    /// files, so this drives `imported_results::load` directly.)
    #[test]
    fn import_mode_marks_the_rows_of_a_dirty_checkout_dirty() {
        use hermit_manifest_plan::imported_results;
        use hermit_manifest_plan::imported_results::bucket_dir;
        use hermit_manifest_plan::runner::RunContext;
        use serde_json::json;

        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-import-dirty-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&fixture).unwrap();
        write_import_fixture(&fixture);
        let manifests = ManifestSet::load(&fixture).unwrap();
        let selection = parse(
            ["--category", "imported", "--ci-only"]
                .into_iter()
                .map(String::from),
        );
        let cells = run_cells(&manifests, &selection).unwrap();
        let clean = RunContext::for_import(fixture.clone(), Some(SHA)).unwrap();
        let import = fixture.join("import");
        let bucket = bucket_dir(&import, "portable", "imported");
        fs::create_dir_all(&bucket).unwrap();
        let (rows, evidence): (Vec<_>, Vec<_>) = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let row =
                    import_producer_row(&clean, cell, format!("buck-producer-{index}"), 1, "PASS");
                (serde_json::to_string(&row).unwrap(), import_execution(&row))
            })
            .unzip();
        fs::write(bucket.join("results.jsonl"), rows.join("\n") + "\n").unwrap();
        fs::write(
            bucket.join("summary.json"),
            serde_json::to_vec(&json!({"evidence_complete_executions": evidence})).unwrap(),
        )
        .unwrap();
        let earns_retry = |_: &SelectedCell, _: &CellResult| false;
        let host_inapplicable = |_: &SelectedCell| None::<String>;
        let policy = imported_results::ImportPolicy {
            earns_retry: &earns_retry,
            host_inapplicable: &host_inapplicable,
        };
        for dirty in [false, true] {
            let mut context = clean.clone();
            context.source_dirty = dirty;
            let imported = imported_results::load(
                &import,
                &cells,
                &context,
                &fixture.join("out/results.jsonl"),
                &policy,
            )
            .unwrap();
            let published = imported
                .cells
                .iter()
                .flat_map(|cell| &cell.rows)
                .map(|row| (row.outcome.as_str(), row.source_tree_dirty))
                .collect::<Vec<_>>();
            assert_eq!(
                published,
                vec![("PASS", dirty); 3],
                "dirty checkout: {dirty}"
            );
        }
        fs::remove_dir_all(&fixture).unwrap();
    }

    /// The parity post-pass of <https://github.com/rrnewton/hermit/issues/3301>
    /// reads only what determinism left behind. With the post-pass off
    /// (`E2E_PARITY_POST_PASS=0`), with it on over the fixture's parity
    /// population (both tests' ptrace verify cells are required, so all
    /// eight candidate cells, the three disabled ones included; the explicit
    /// five add nothing to it), and with a planted mutation that makes every
    /// compared candidate diverge from ptrace, the harness exits the same way
    /// and writes the same `results.jsonl`, JUnit and `summary.json` (apart
    /// from wall and CPU timings), and it runs the same guest executions: the
    /// only extra processes are one `hermit log-diff` per measurable cell. That holds
    /// for a run that exits 1 and for one that exits 0, and when the
    /// post-pass fails, panics or finds the enclosing step's bound already
    /// spent. Without `E2E_KEEP_VERIFY_LOGS`, the rows differ from a
    /// parity-off run only by the `--keep-logs --verify-log-dir <dir>` that
    /// retains the compared cells' logs.
    #[test]
    fn the_parity_post_pass_changes_no_determinism_output_and_runs_no_guest() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::path::PathBuf;
        use std::process::Command;

        use dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV;
        use hermit_manifest_plan::parity::ParityRecord;
        use hermit_manifest_plan::parity::ParityVerdict;
        use hermit_manifest_plan::parity::PostPassState;
        use hermit_manifest_plan::parity::PostPassStatus;
        use hermit_manifest_plan::runner::CellResult;
        use serde_json::Value;
        use serde_json::json;

        const CHILD: &str = "HERMIT_PARITY_POST_PASS_FIXTURE";
        const OUT: &str = "HERMIT_PARITY_POST_PASS_OUT";
        const COMPARE: &str = "HERMIT_PARITY_POST_PASS_COMPARE";
        const TEST: &str =
            "tests::the_parity_post_pass_changes_no_determinism_output_and_runs_no_guest";
        const SELECT: &str = "parity/alpha@dbt,parity/alpha@kvm,parity/alpha@liteinst,\
                              parity/beta@kvm,parity/beta@liteinst";
        if let Some(fixture) = std::env::var_os(CHILD) {
            let fixture = PathBuf::from(fixture);
            let out = PathBuf::from(std::env::var_os(OUT).unwrap());
            let manifests = ManifestSet::load(&fixture).unwrap();
            let code = if let Some(cells) = std::env::var_os(COMPARE) {
                super::parity_compare(
                    &fixture,
                    &manifests,
                    &super::ParityCompareRequest {
                        artifacts: out.clone(),
                        cells: cells
                            .to_str()
                            .unwrap()
                            .split(',')
                            .map(str::to_string)
                            .collect(),
                        output: None,
                        jobs: 1,
                    },
                )
            } else {
                let values = vec![
                    "--category".into(),
                    "parity".into(),
                    "--ci-only".into(),
                    "--prebuilt".into(),
                    "--jobs".into(),
                    "1".into(),
                    "--results".into(),
                    out.join("results.jsonl").display().to_string(),
                    "--junit".into(),
                    out.join("junit.xml").display().to_string(),
                ];
                let args = parse(values.into_iter());
                validate_args("run", &args);
                super::run(&fixture, &manifests, &args)
            };
            fs::write(out.join("exit"), format!("{code:?}")).unwrap();
            return;
        }

        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-parity-post-pass-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&fixture);
        let manifests = fixture.join("tests/e2e/manifests");
        fs::create_dir_all(&manifests).unwrap();
        // The fixture is the checkout every child runs the harness in: a git
        // repository with one empty commit, so each child's provenance probe
        // (`git rev-parse HEAD`, `git status`) has no tracked file to stat.
        // Run in the real checkout, `git status` stats every tracked file,
        // agent-utils' included, once per child. `maintenance.auto=false`
        // stops the commit from leaving a detached `git maintenance run
        // --auto` behind, holding a lock inside the fixture that this test
        // removes at the end.
        for args in [
            &["init", "-q"][..],
            &[
                "-c",
                "maintenance.auto=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        ] {
            let output = Command::new("git")
                .arg("-C")
                .arg(&fixture)
                .args(args)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_NAME", "fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        }
        fs::write(
            manifests.join("defaults.yaml"),
            "schema: 3\ntimeout_seconds: 10\ncpu_timeout_seconds: 5\n",
        )
        .unwrap();
        let disabled = json!({"ci": false, "backends_enabled": [], "backends_disabled": {
            "ptrace": "Not selected by this control", "dbt": "Not selected by this control",
            "kvm": "Not selected by this control", "sabre": "Not selected by this control",
            "liteinst": "Not selected by this control",
            "in-guest-trap": "Not selected by this control"
        }});
        let naked = json!({"ci": false, "backends_enabled": [],
            "backends_disabled": {"native": "Not selected by this CI control"}});
        let test = |id: &str, enabled: Value, off: Value| {
            json!({
                "id": id, "description": "Parity post-pass control", "lane": "portable",
                "occasional": false, "direct": ["/bin/true"],
                "observation": {"status": true, "stdout": true, "stderr": true},
                "modes": {
                    "verify": {"ci": true, "backends_enabled": enabled, "backends_disabled": off},
                    "naked": naked, "chaos": disabled, "replay": disabled,
                    "custom": disabled
                }
            })
        };
        fs::write(
            manifests.join("parity.yaml"),
            serde_json::to_vec(&json!({"schema": 3, "bucket": "parity", "test": [
                test("parity/alpha", json!(["ptrace", "kvm", "liteinst", "dbt"]),
                    json!({"sabre": "Not selected", "in-guest-trap": "Not selected"})),
                test("parity/beta", json!(["ptrace", "kvm", "liteinst"]),
                    json!({"dbt": "Not selected", "sabre": "Not selected",
                        "in-guest-trap": "Not selected"})),
            ]}))
            .unwrap(),
        )
        .unwrap();
        let output = json!({"exit_code": 0, "signal": null,
            "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stdout_bytes": 0, "stderr_bytes": 0});
        let comparison = json!({"strictness": "canonical", "display_name": "BitwiseInfoV1",
            "compare_logs": true, "compare_io_buffers": true, "log_scope": "info",
            "record_envelope": "all_records_v1", "virtualize_time": true,
            "strip_lines": false, "canonicalize_addresses": true, "full_trace": true,
            "exact_remainder": true, "stripped_prefixes": ["real-wall-clock-prefix/v1"],
            "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
            "ignore_lines": false, "skip_commit": false, "skip_detlog": false});
        let report = |verdict: &str| {
            let diverged = verdict == "diverged";
            json!({
                "verified": !diverged, "bitwise_parity": !diverged, "verdict": verdict,
                "no_result_reason": null, "infrastructure_error": null,
                "comparison": comparison,
                "compared_log_messages": {"left": 2, "right": 2},
                "compared_outputs": {"left": output, "right": output},
                "guest_exit_code": 0, "guest_signal": null,
                "first_divergent_scheduler_turn": if diverged { json!(4) } else { Value::Null },
                "first_divergent_virtual_nanoseconds": if diverged { json!(7) } else { Value::Null },
                "first_divergent_record": if diverged { json!(2) } else { Value::Null },
                "first_divergent_syscall": if diverged { json!(1) } else { Value::Null },
                "first_divergent_left_message": if diverged { json!("left") } else { Value::Null },
                "first_divergent_right_message": if diverged { json!("right") } else { Value::Null }
            })
        };
        for verdict in ["matched", "diverged"] {
            fs::write(
                fixture.join(format!("verification-{verdict}.json")),
                serde_json::to_vec(&report(verdict)).unwrap(),
            )
            .unwrap();
        }
        fs::write(
            fixture.join("fake-parity-log-diff.py"),
            include_str!("../../tests/fixtures/fake-parity-log-diff.py"),
        )
        .unwrap();
        // One fake hermit: `run` is a guest execution and is counted; a
        // single-log `log-diff`, the ptrace golden normalization removed for
        // https://github.com/rrnewton/hermit/issues/3301, is counted so that
        // its return fails the test; a two-log `log-diff` is a parity
        // comparison. The scenario is a list of words:
        // parity/beta's liteinst cell fails determinism unless it has `pass`,
        // and `mutated` changes every candidate's second detcore message.
        let hermit = fixture.join("hermit");
        // This fake starts at every probe, guest run and `log-diff` of the
        // test, so it imports only `sys` and, past the probes, `os`. `-IS`
        // skips the site-packages scan at every start, the `--help` and
        // version probes exit before `os` is imported, and each invocation
        // record is written as the literal line `json.dumps` produces for
        // it: the cell directory names are ASCII slugs with no quote or
        // backslash. Importing json and pathlib cost more than the rest of
        // the script. The log-diff stand-in starts with `-IS` too.
        fs::write(
            &hermit,
            r#"#!/usr/bin/python3 -IS
import sys
full=sys.argv[1:]
a=full
if '--help' in a:
 print('--verify-strict');sys.exit(0)
if 'run' in a:a=a[a.index('run'):]
if not a or a[0] not in ('run','log-diff'):sys.exit(0)
import os
root=os.path.dirname(os.path.realpath(__file__))
def record(line):
 with open(os.path.join(root,'invocations'),'a') as f:f.write(line+'\n')
if a[0]=='log-diff':
 if len(a)==2:
  record('{"kind": "normalize"}');sys.stdout.buffer.write(open(a[1],'rb').read());sys.exit(0)
 record('{"kind": "compare"}')
 os.execv(sys.executable,[sys.executable,'-IS',os.path.join(root,'fake-parity-log-diff.py')]+a)
# `--backend` is a global option, so it precedes `run`.
backend=full[full.index('--backend')+1]
report=a[a.index('--verify-json')+1]
cell=os.path.basename(os.path.dirname(report))
record('{"kind": "run", "cell": "%s"}'%cell)
scenario=open(os.path.join(root,'scenario')).read().split()
assert '--verify-strict' in a and '--verify' in a,a
if '--verify-log-dir' in a:
 logdir=a[a.index('--verify-log-dir')+1]
 os.makedirs(logdir,exist_ok=True)
 mutated='mutated' in scenario and backend!='ptrace'
 read='read 4 on '+backend if mutated else 'read 3'
 with open(os.path.join(logdir,'run1_log_fixture.log'),'w') as f:f.write('INFO detcore: open\nINFO detcore: '+read+'\nINFO detcore: exit 0\n')
failed=cell.startswith('parity-beta-verify-liteinst') and 'pass' not in scenario
data=open(os.path.join(root,'verification-diverged.json' if failed else 'verification-matched.json'),'rb').read()
with open(report,'wb') as f:f.write(data)
sys.exit(1 if failed else 0)
"#,
        )
        .unwrap();
        fs::set_permissions(&hermit, fs::Permissions::from_mode(0o755)).unwrap();

        let artifacts = fixture.join("artifacts");
        let child = |label: &str,
                     scenario: &str,
                     keep: bool,
                     select: Option<&str>,
                     compare: Option<&str>,
                     extra: &[(&str, &str)]| {
            let out = fixture.join(format!("out-{label}"));
            fs::create_dir_all(&out).unwrap();
            fs::write(fixture.join("scenario"), scenario).unwrap();
            if compare.is_none() {
                // Fresh cell directories under the same paths, so every run's
                // rows name the same artifact directories and logs.
                let _ = fs::remove_dir_all(&artifacts);
            }
            let _ = fs::remove_file(fixture.join("invocations"));
            let mut command = Command::new("timeout");
            command
                .args(["--kill-after=2s", "60s"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env(CHILD, &fixture)
                .env(OUT, &out)
                .env("HERMIT_BIN", &hermit)
                .env("E2E_RESULT_ROOT", &artifacts)
                .env("E2E_BUILD_ROOT", fixture.join("build"))
                .env("E2E_RUN_ID", "parity-post-pass-control")
                // One comparison epoch for every run, so HERMIT_EPOCH in each
                // row's environment is the same across runs.
                .env("HERMIT_EPOCH", "2021-12-31T23:59:59Z")
                .env("E2E_MACHINE_SHORTNAME", "native-control")
                .env("E2E_KERNEL_VERSION", "native-control")
                // The fixture's cells name backends that need KVM or CPUID
                // faulting; without this a host lacking either reports them
                // HOST-INAPPLICABLE and the exit status this test pins changes.
                // The child runs no guest, so forcing them present is safe.
                .env(
                    "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT",
                    "kvm,cpuid-faulting",
                );
            if keep {
                command.env("E2E_KEEP_VERIFY_LOGS", "1");
            }
            if let Some(select) = select {
                command.env("E2E_PARITY_SELECT", select);
            }
            if let Some(cells) = compare {
                command.env(COMPARE, cells);
            }
            command.envs(extra.iter().copied());
            let result = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&result.stderr).into_owned();
            fs::write(out.join("child.stdout"), &stdout).unwrap();
            fs::write(out.join("child.stderr"), &stderr).unwrap();
            let calls = fs::read_to_string(fixture.join("invocations"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            (out, result.status, stdout, stderr, calls)
        };
        let count =
            |calls: &[Value], kind: &str| calls.iter().filter(|call| call["kind"] == kind).count();
        let guest_runs = |calls: &[Value]| {
            let mut runs = calls
                .iter()
                .filter(|call| call["kind"] == "run")
                .map(|call| call["cell"].as_str().unwrap().to_string())
                .collect::<Vec<_>>();
            runs.sort();
            runs
        };
        let records = |path: &Path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<ParityRecord>(line).unwrap())
                .collect::<Vec<_>>()
        };
        let rows = |out: &Path| {
            fs::read_to_string(out.join("results.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
                .collect::<Vec<_>>()
        };
        // The determinism outputs with only wall and CPU timings removed.
        let without_timings = |out: &Path| {
            fn strip(value: &mut Value) {
                if let Value::Object(map) = value {
                    for key in ["duration_ms", "cpu_usage_usec", "cpu_observations"] {
                        map.remove(key);
                    }
                    map.values_mut().for_each(strip);
                } else if let Value::Array(items) = value {
                    items.iter_mut().for_each(strip);
                }
            }
            let results = fs::read_to_string(out.join("results.jsonl"))
                .unwrap()
                .lines()
                .map(|line| {
                    let mut value: Value = serde_json::from_str(line).unwrap();
                    strip(&mut value);
                    value
                })
                .collect::<Vec<_>>();
            let junit = fs::read_to_string(out.join("junit.xml")).unwrap();
            let junit = junit
                .split(" time=\"")
                .enumerate()
                .map(|(index, part)| {
                    if index == 0 {
                        part.to_string()
                    } else {
                        part.split_once('"').unwrap().1.to_string()
                    }
                })
                .collect::<String>();
            let mut summary: Value =
                serde_json::from_slice(&fs::read(out.join("summary.json")).unwrap()).unwrap();
            summary
                .as_object_mut()
                .unwrap()
                .remove("cell_cpu_usage_usec");
            (
                results,
                junit,
                summary,
                fs::read_to_string(out.join("exit")).unwrap(),
            )
        };
        let verdicts = |records: &[ParityRecord]| {
            records
                .iter()
                .map(|record| format!("{}@{}={:?}", record.test_id, record.backend, record.verdict))
                .collect::<Vec<_>>()
        };
        // `without_timings` output with exactly the `--keep-logs
        // --verify-log-dir <dir>` that retains a cell's logs removed from each
        // row's argv, effective_args and shell_command, and from each
        // attempt's argv and shell_command, and the number of rows that had it.
        fn drop_kept_logs(value: &mut Value, rows: &mut usize) {
            const KEEP: [&str; 2] = ["--keep-logs", "--verify-log-dir"];
            fn drop_triple(args: &mut Vec<Value>) -> Option<String> {
                let at = args.iter().position(|arg| arg == KEEP[0])?;
                assert_eq!(args.get(at + 1).and_then(Value::as_str), Some(KEEP[1]));
                let dir = args[at + 2].as_str().unwrap().to_string();
                args.drain(at..at + 3);
                assert!(
                    !args
                        .iter()
                        .any(|arg| KEEP.contains(&arg.as_str().unwrap_or("")))
                );
                Some(dir)
            }
            if let Value::Object(map) = value {
                let dir = map
                    .get_mut("argv")
                    .and_then(Value::as_array_mut)
                    .and_then(drop_triple);
                if let Some(dir) = dir {
                    let quoted = hermit_manifest_plan::runner::shell_command(
                        ".",
                        &BTreeMap::new(),
                        std::slice::from_ref(&dir),
                    );
                    let segment = format!(
                        " {} {} {}",
                        KEEP[0],
                        KEEP[1],
                        quoted.strip_prefix("cd . && env ").unwrap()
                    );
                    let command = map["shell_command"].as_str().unwrap();
                    assert_eq!(command.matches(&segment).count(), 1, "{command}");
                    map["shell_command"] = Value::String(command.replacen(&segment, "", 1));
                    if let Some(args) = map.get_mut("effective_args") {
                        *rows += 1;
                        let effective = drop_triple(args.as_array_mut().unwrap());
                        assert_eq!(effective.as_deref(), Some(dir.as_str()));
                    }
                } else if let Some(command) = map.get("shell_command").and_then(Value::as_str) {
                    assert!(!command.contains(KEEP[1]), "{command}");
                }
                map.values_mut()
                    .for_each(|value| drop_kept_logs(value, rows));
            } else if let Value::Array(items) = value {
                items
                    .iter_mut()
                    .for_each(|value| drop_kept_logs(value, rows));
            }
        }
        let without_kept_logs = |mut outputs: (Vec<Value>, String, Value, String)| {
            let mut rows = 0;
            outputs
                .0
                .iter_mut()
                .for_each(|row| drop_kept_logs(row, &mut rows));
            (outputs, rows)
        };
        // Every file below `dir`, by path relative to it.
        let tree = |dir: &Path| {
            fn walk(base: &Path, dir: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
                for entry in fs::read_dir(dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        walk(base, &path, files);
                    } else {
                        let relative = path.strip_prefix(base).unwrap().display().to_string();
                        files.insert(relative, fs::read(&path).unwrap());
                    }
                }
            }
            let mut files = BTreeMap::new();
            walk(dir, dir, &mut files);
            files
        };
        let sha256_hex = |bytes: &[u8]| {
            use sha2::Digest;
            sha2::Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };

        // Eight guest executions: seven verify cells, and the failing cell's
        // one retry.
        let expected_runs = {
            let mut runs = [
                "parity-alpha-verify-dbt",
                "parity-alpha-verify-kvm",
                "parity-alpha-verify-liteinst",
                "parity-alpha-verify-ptrace",
                "parity-beta-verify-kvm",
                "parity-beta-verify-liteinst",
                "parity-beta-verify-liteinst-attempt-2",
                "parity-beta-verify-ptrace",
            ]
            .map(str::to_string)
            .to_vec();
            runs.sort();
            runs
        };

        // The post-pass off: the harness reports no parity cell at all.
        let off: &[(&str, &str)] = &[("E2E_PARITY_POST_PASS", "0")];
        let (none, _, none_stdout, _, none_calls) = child("none", "shared", true, None, None, off);
        let (five, _, five_stdout, _, five_calls) =
            child("five", "shared", true, Some(SELECT), None, &[]);
        let five_records = records(&five.join("parity.jsonl"));
        let (mutated, _, mutated_stdout, _, mutated_calls) =
            child("mutated", "mutated", true, Some(SELECT), None, &[]);
        let mutated_records = records(&mutated.join("parity.jsonl"));

        let baseline = without_timings(&none);
        assert_eq!(baseline.3, "ExitCode(unix_exit_status(1))", "{none_stdout}");
        assert_eq!(
            without_timings(&five),
            baseline,
            "five cells changed a determinism output"
        );
        assert_eq!(
            without_timings(&mutated),
            baseline,
            "a planted divergence changed a determinism output"
        );
        assert_eq!(
            rows(&none)
                .iter()
                .map(|row| (
                    row.test.clone(),
                    row.backend.clone(),
                    row.attempt,
                    row.outcome.clone()
                ))
                .collect::<Vec<_>>(),
            rows(&five)
                .iter()
                .map(|row| (
                    row.test.clone(),
                    row.backend.clone(),
                    row.attempt,
                    row.outcome.clone()
                ))
                .collect::<Vec<_>>(),
        );
        for calls in [&none_calls, &five_calls, &mutated_calls] {
            assert_eq!(
                guest_runs(calls),
                expected_runs,
                "no additional guest execution"
            );
            assert_eq!(
                count(calls, "normalize"),
                0,
                "E2E_KEEP_VERIFY_LOGS retains logs and launches no golden-log normalization"
            );
        }
        assert_eq!(count(&none_calls, "compare"), 0);
        assert!(!none.join("parity.jsonl").exists());
        assert!(!none.join("parity").exists());
        assert!(
            !none_stdout.contains("test-harness: parity:"),
            "{none_stdout}"
        );
        // alpha@dbt, alpha@kvm, alpha@liteinst and beta@kvm are measurable; a
        // candidate that failed determinism is not compared, and neither is a
        // disabled one (alpha@in-guest-trap, alpha@sabre, beta@dbt,
        // beta@in-guest-trap, beta@sabre), which full validation never runs.
        assert_eq!(count(&five_calls, "compare"), 4);
        assert_eq!(count(&mutated_calls, "compare"), 4);
        assert!(
            five_stdout.contains("test-harness: parity: 10 cell(s)"),
            "{five_stdout}"
        );
        // Every one of the 10 is in the population: the 4 measured matches
        // score 1, the nondeterministic candidate and the 5 disabled ones 0.
        assert!(
            five_stdout.contains(
                "population 10: mean credit 0.4000 over 10 (4 full, 0 partial, 6 zero); 0 excluded \
                 (ptrace reference left no golden)"
            ),
            "{five_stdout}"
        );
        assert!(
            five_stdout.contains(
                "measured 4; no golden 1 (determinism-mismatch 1); not compared 0; unmeasured 0; \
                 candidate not run 5 (candidate-not-enabled 5); mean credit 1.0000 over 4 \
                 measured with equal inputs; none measured with unequal inputs"
            ),
            "{five_stdout}"
        );
        assert!(
            mutated_stdout.contains("4 log-diff comparison(s), 0 guest runs"),
            "{mutated_stdout}"
        );
        assert_eq!(
            verdicts(&five_records),
            [
                "parity/alpha@dbt=Matched",
                "parity/alpha@in-guest-trap=CandidateMissing",
                "parity/alpha@kvm=Matched",
                "parity/alpha@liteinst=Matched",
                "parity/alpha@sabre=CandidateMissing",
                "parity/beta@dbt=CandidateMissing",
                "parity/beta@in-guest-trap=CandidateMissing",
                "parity/beta@kvm=Matched",
                "parity/beta@liteinst=Nondeterministic",
                "parity/beta@sabre=CandidateMissing",
            ]
        );
        assert!(
            five_records[8]
                .reason
                .as_deref()
                .unwrap()
                .contains("failed determinism"),
            "{:?}",
            five_records[8]
        );
        // A disabled candidate carries its typed class and the manifest's
        // own disabled reason.
        for index in [1, 4, 5, 6, 9] {
            let record = &five_records[index];
            assert_eq!(
                record.unavailable_class,
                Some(hermit_manifest_plan::parity::UnavailableClass::CandidateNotEnabled),
                "{record:?}"
            );
            assert!(
                record.reason.as_deref().unwrap().ends_with(
                    "is not enabled in the manifest, so it has no log to compare: \
                                Not selected"
                ),
                "{record:?}"
            );
        }
        assert_eq!(
            verdicts(&mutated_records),
            [
                "parity/alpha@dbt=Diverged",
                "parity/alpha@in-guest-trap=CandidateMissing",
                "parity/alpha@kvm=Diverged",
                "parity/alpha@liteinst=Diverged",
                "parity/alpha@sabre=CandidateMissing",
                "parity/beta@dbt=CandidateMissing",
                "parity/beta@in-guest-trap=CandidateMissing",
                "parity/beta@kvm=Diverged",
                "parity/beta@liteinst=Nondeterministic",
                "parity/beta@sabre=CandidateMissing",
            ]
        );
        // The harness launched every candidate verify cell, dbt included, and
        // their ptrace reference, with the equalized guest inputs, so each
        // measured comparison is clean credit. A cell refused before
        // comparing is not shown to have them.
        for records in [&five_records, &mutated_records] {
            for (index, record) in records.iter().enumerate() {
                record.validate().unwrap();
                let measured = [0, 2, 3, 7].contains(&index);
                assert_eq!(record.verdict.is_measured(), measured, "{record:?}");
                assert_eq!(record.inputs_equalized, measured, "{record:?}");
                assert_eq!(record.unequalized_credit, None, "{record:?}");
                if !measured {
                    assert_eq!(record.credit, None, "{record:?}");
                }
            }
        }
        for record in five_records
            .iter()
            .filter(|record| record.verdict.is_measured())
        {
            assert_eq!(record.credit, Some(1.0), "{record:?}");
        }
        for record in mutated_records
            .iter()
            .filter(|record| record.verdict.is_measured())
        {
            assert_eq!(record.matched_prefix, Some(1), "{record:?}");
            assert_eq!(record.first_divergent_record, Some(2), "{record:?}");
            assert_eq!(record.credit, Some(1.0 / 3.0), "{record:?}");
            let difference = record.first_difference.as_ref().unwrap();
            assert_eq!(
                difference.reference_message.as_deref(),
                Some("INFO detcore: read 3")
            );
            assert_eq!(
                difference.candidate_message.as_deref(),
                Some(format!("INFO detcore: read 4 on {}", record.backend).as_str())
            );
        }

        // A later run into the same directory with no parity cell in scope
        // removes the earlier report, so it cannot be read as that run's.
        let five_status: PostPassStatus =
            serde_json::from_slice(&fs::read(five.join("parity.status.json")).unwrap()).unwrap();
        assert_eq!(five_status.state, PostPassState::Complete);
        assert_eq!(five_status.cells, 10);
        assert!(five.join("parity/logdiff").is_dir());
        let (again, _, _, _, again_calls) = child("five", "shared", true, None, None, off);
        assert_eq!(count(&again_calls, "compare"), 0);
        for stale in ["parity.jsonl", "parity.status.json", "parity/logdiff"] {
            assert!(!again.join(stale).exists(), "{stale} survived");
        }

        // A run in which every cell passes determinism exits 0, with parity
        // off and with every compared candidate diverging from ptrace.
        let passing_runs = expected_runs
            .iter()
            .filter(|run| !run.ends_with("-attempt-2"))
            .cloned()
            .collect::<Vec<_>>();
        let (pass_off, _, pass_off_stdout, _, pass_off_calls) =
            child("pass-off", "pass", true, None, None, off);
        let pass_baseline = without_timings(&pass_off);
        assert_eq!(
            pass_baseline.3, "ExitCode(unix_exit_status(0))",
            "{pass_off_stdout}"
        );
        assert_eq!(count(&pass_off_calls, "compare"), 0);
        let (pass, _, pass_stdout, pass_stderr, pass_calls) =
            child("pass", "pass mutated", true, Some(SELECT), None, &[]);
        assert_eq!(
            without_timings(&pass),
            pass_baseline,
            "a planted divergence changed a passing run's determinism output\n\
             {pass_stdout}\n{pass_stderr}"
        );
        for calls in [&pass_off_calls, &pass_calls] {
            assert_eq!(
                guest_runs(calls),
                passing_runs,
                "no additional guest execution"
            );
        }
        assert_eq!(count(&pass_calls, "compare"), 5);
        assert_eq!(
            verdicts(&records(&pass.join("parity.jsonl"))),
            [
                "parity/alpha@dbt=Diverged",
                "parity/alpha@in-guest-trap=CandidateMissing",
                "parity/alpha@kvm=Diverged",
                "parity/alpha@liteinst=Diverged",
                "parity/alpha@sabre=CandidateMissing",
                "parity/beta@dbt=CandidateMissing",
                "parity/beta@in-guest-trap=CandidateMissing",
                "parity/beta@kvm=Diverged",
                "parity/beta@liteinst=Diverged",
                "parity/beta@sabre=CandidateMissing",
            ]
        );

        // A post-pass that fails (its records path is a directory it cannot
        // replace), that panics, or that starts after the enclosing dagrun
        // step's bound is spent, leaves the passing run's exit status and
        // determinism outputs as they are, and says so.
        fs::create_dir_all(fixture.join("out-failed/parity.jsonl")).unwrap();
        let (failed, _, _, failed_stderr, failed_calls) =
            child("failed", "pass mutated", true, Some(SELECT), None, &[]);
        let (panicked, _, _, panicked_stderr, panicked_calls) = child(
            "panicked",
            "pass mutated",
            true,
            Some(SELECT),
            None,
            &[("HERMIT_PARITY_POST_PASS_PANIC", "1")],
        );
        for (out, stderr, calls, message) in [
            (
                &failed,
                &failed_stderr,
                &failed_calls,
                "test-harness: parity post-pass failed (exit status unaffected): cannot remove",
            ),
            (
                &panicked,
                &panicked_stderr,
                &panicked_calls,
                "test-harness: parity post-pass panicked (exit status unaffected)",
            ),
        ] {
            assert_eq!(without_timings(out), pass_baseline, "{stderr}");
            assert!(stderr.contains(message), "{stderr}");
            assert_eq!(guest_runs(calls), passing_runs);
            assert_eq!(count(calls, "compare"), 0);
        }
        assert!(panicked_stderr.contains("planted harness post-pass panic"));
        assert!(failed.join("parity.jsonl").is_dir());
        let failed_status: PostPassStatus =
            serde_json::from_slice(&fs::read(failed.join("parity.status.json")).unwrap()).unwrap();
        assert_eq!(failed_status.state, PostPassState::Failed);
        assert!(
            failed_status
                .error
                .as_deref()
                .unwrap()
                .starts_with("cannot remove"),
            "{failed_status:?}"
        );
        assert!(!panicked.join("parity.jsonl").exists());
        let (expired, _, _, expired_stderr, expired_calls) = child(
            "expired",
            "pass mutated",
            true,
            Some(SELECT),
            None,
            &[(STEP_STARTED_MONOTONIC_NS_ENV, &u64::MAX.to_string())],
        );
        assert_eq!(without_timings(&expired), pass_baseline, "{expired_stderr}");
        assert_eq!(guest_runs(&expired_calls), passing_runs);
        assert_eq!(count(&expired_calls, "compare"), 0);
        let expired_records = records(&expired.join("parity.jsonl"));
        // A disabled candidate is decided before any comparison, so the spent
        // bound leaves it candidate-missing; every comparison is unavailable.
        assert_eq!(
            verdicts(&expired_records),
            [
                "parity/alpha@dbt=Unavailable",
                "parity/alpha@in-guest-trap=CandidateMissing",
                "parity/alpha@kvm=Unavailable",
                "parity/alpha@liteinst=Unavailable",
                "parity/alpha@sabre=CandidateMissing",
                "parity/beta@dbt=CandidateMissing",
                "parity/beta@in-guest-trap=CandidateMissing",
                "parity/beta@kvm=Unavailable",
                "parity/beta@liteinst=Unavailable",
                "parity/beta@sabre=CandidateMissing",
            ]
        );
        for record in expired_records
            .iter()
            .filter(|record| record.verdict == ParityVerdict::Unavailable)
        {
            let reason = record.reason.as_deref().unwrap();
            assert!(
                reason.contains("the enclosing dagrun step's unknown wall bound")
                    && reason.contains("ran out before this comparison started"),
                "{record:?}"
            );
        }

        // Without E2E_KEEP_VERIFY_LOGS, the population closure's logs are
        // still retained, the golden and its sidecar are written, and nothing
        // is normalized. Against the same run with parity off, every row is
        // the same apart from timings and the `--keep-logs --verify-log-dir
        // <dir>` that retains a compared cell's logs, which is in its argv,
        // effective_args and shell_command and in each attempt's argv and
        // shell_command.
        let (retained_off, _, retained_off_stdout, _, retained_off_calls) =
            child("retained-off", "shared", false, None, None, off);
        let (retained, _, _, _, retained_calls) =
            child("retained", "shared", false, Some(SELECT), None, &[]);
        for calls in [&retained_off_calls, &retained_calls] {
            assert_eq!(guest_runs(calls), expected_runs);
            assert_eq!(count(calls, "normalize"), 0);
        }
        assert_eq!(count(&retained_off_calls, "compare"), 0);
        assert_eq!(count(&retained_calls, "compare"), 4);
        assert!(!retained_off.join("parity.jsonl").exists());
        let retained_off_outputs = without_timings(&retained_off);
        assert_eq!(retained_off_outputs.3, baseline.3, "{retained_off_stdout}");
        assert_eq!(without_kept_logs(retained_off_outputs.clone()).1, 0);
        let (retained_outputs, kept_rows) = without_kept_logs(without_timings(&retained));
        // Eight rows keep logs: every row, beta@liteinst's twice.
        assert_eq!(kept_rows, 8);
        assert_eq!(
            retained_outputs, retained_off_outputs,
            "retaining the parity closure's logs changed a determinism output"
        );
        for row in rows(&retained) {
            let kept = row.argv.iter().any(|arg| arg == "--verify-log-dir");
            let backend = row.backend.as_deref().unwrap();
            assert!(kept, "{} {backend}: {:?}", row.test, row.argv);
        }
        assert_eq!(
            verdicts(&records(&retained.join("parity.jsonl"))),
            verdicts(&five_records)
        );
        for test in ["alpha", "beta"] {
            let golden = retained.join(format!("parity/golden/parity/{test}.detlog"));
            let sidecar: hermit_manifest_plan::parity::ParityGoldenSidecar =
                serde_json::from_slice(
                    &fs::read(retained.join(format!("parity/golden/parity/{test}.inputs.json")))
                        .unwrap(),
                )
                .unwrap();
            let bytes = fs::read(&golden).unwrap();
            assert_eq!(
                bytes,
                b"INFO detcore: open\nINFO detcore: read 3\nINFO detcore: exit 0\n"
            );
            assert_eq!(sidecar.log_bytes, bytes.len() as u64);
            assert_eq!(sidecar.backend, "ptrace");
            assert_eq!(sidecar.run_id, "parity-post-pass-control");
            assert_eq!(sidecar.guest_inputs.guest_argv, ["/bin/true"]);
            assert_eq!(
                sidecar.guest_inputs.epoch.as_deref(),
                Some("2021-12-31T23:59:59Z")
            );
        }

        // A retained candidate log deleted afterwards is candidate-missing,
        // measured again by `parity compare` from the same run's files alone,
        // which it leaves byte-identical.
        let snapshot = |out: &Path| {
            let files = [
                "results.jsonl",
                "junit.xml",
                "summary.json",
                "parity.jsonl",
                "parity.status.json",
            ]
            .map(|name| fs::read(out.join(name)).unwrap());
            (files, tree(&out.join("parity")))
        };
        let before = snapshot(&retained);
        let beta_kvm = rows(&retained)
            .into_iter()
            .find(|row| row.test == "parity/beta" && row.backend.as_deref() == Some("kvm"))
            .unwrap();
        let logs = &beta_kvm.argv[beta_kvm
            .argv
            .iter()
            .position(|arg| arg == "--verify-log-dir")
            .unwrap()
            + 1];
        fs::remove_file(Path::new(logs).join("run1_log_fixture.log")).unwrap();
        let (_, status, compare_stdout, compare_stderr, compare_calls) = child(
            "retained",
            "shared",
            false,
            None,
            Some("parity/beta@kvm,parity/alpha@kvm"),
            &[],
        );
        assert!(status.success(), "{compare_stdout}\n{compare_stderr}");
        assert_eq!(
            snapshot(&retained),
            before,
            "parity compare changed a run output"
        );
        assert_eq!(guest_runs(&compare_calls), Vec::<String>::new());
        assert_eq!(count(&compare_calls, "compare"), 1);
        let compare_dir = retained.join("parity-compare");
        let compared = records(&compare_dir.join("parity.jsonl"));
        let compare_status: PostPassStatus =
            serde_json::from_slice(&fs::read(compare_dir.join("parity.status.json")).unwrap())
                .unwrap();
        assert_eq!(compare_status.state, PostPassState::Complete);
        assert_eq!(compare_status.cells, 2);
        assert_eq!(compare_status.hermit_bin, hermit.display().to_string());
        assert_eq!(
            compare_status.hermit_bin_sha256.as_deref(),
            Some(sha256_hex(&fs::read(&hermit).unwrap()).as_str())
        );
        // Below its own directory it wrote only the goldens it took from the
        // retained ptrace logs, the same files the run wrote, and its one
        // comparison's log-diff report.
        let written = tree(&compare_dir.join("parity"));
        let run_parity = tree(&retained.join("parity"));
        let goldens = written
            .iter()
            .filter(|(path, _)| path.starts_with("golden/"))
            .collect::<BTreeMap<_, _>>();
        assert!(
            goldens.contains_key(&"golden/parity/alpha.detlog".to_string()),
            "{:?}",
            written.keys()
        );
        for (path, bytes) in &goldens {
            assert_eq!(run_parity.get(*path), Some(*bytes), "{path}");
        }
        assert_eq!(
            written
                .keys()
                .filter(|path| !path.starts_with("golden/"))
                .collect::<Vec<_>>(),
            [
                "logdiff/parity/alpha@kvm.json",
                "logdiff/parity/alpha@kvm.stderr"
            ],
            "{:?}",
            written.keys()
        );
        assert_eq!(
            verdicts(&compared),
            [
                "parity/alpha@kvm=Matched",
                "parity/beta@kvm=CandidateMissing"
            ]
        );
        assert_eq!(compared[1].verdict, ParityVerdict::CandidateMissing);
        assert!(
            compared[1]
                .reason
                .as_deref()
                .unwrap()
                .contains("retained 0 run1_log_* logs"),
            "{:?}",
            compared[1]
        );
        assert_eq!(
            compare_stdout
                .lines()
                .filter(|line| line.starts_with("{\"schema\""))
                .count(),
            2,
            "{compare_stdout}"
        );

        // An E2E_PARITY_SELECT the matrix cannot measure is a usage error,
        // refused before any cell runs or any output is written.
        let (refused, status, _, refused_stderr, refused_calls) = child(
            "refused",
            "shared",
            false,
            Some("parity/alpha@ptrace"),
            None,
            &[],
        );
        assert_eq!(status.code(), Some(2), "{refused_stderr}");
        assert!(
            refused_stderr.contains("E2E_PARITY_SELECT"),
            "{refused_stderr}"
        );
        assert!(refused_calls.is_empty(), "{refused_calls:?}");
        assert!(!refused.join("results.jsonl").exists());
        fs::remove_dir_all(&fixture).unwrap();
    }

    /// A closed or full stdout cannot turn the parity summary line into a
    /// panic that changes the exit status.
    #[test]
    fn the_parity_summary_line_ignores_a_stdout_that_cannot_be_written() {
        struct Closed;
        impl std::io::Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        let outcome = std::panic::catch_unwind(|| {
            print_best_effort(Closed, "test-harness: parity: 1 cell(s)");
        });
        assert!(outcome.is_ok(), "a failed write panicked");
        let mut written = Vec::new();
        print_best_effort(&mut written, "test-harness: parity: 1 cell(s)");
        assert_eq!(written, b"test-harness: parity: 1 cell(s)\n");
    }

    /// `parity compare` reads an imported run's logs from the import root its
    /// summary.json records (<https://github.com/rrnewton/hermit/issues/3687>):
    /// the configuration it measures with loads that root's log index. A run
    /// that imported nothing has none; a summary that cannot be read, is not a
    /// JSON object, or records an import without an absolute root, is refused
    /// rather than read as a run that imported nothing.
    #[test]
    fn parity_compare_finds_an_imported_runs_logs_through_its_summary() {
        let artifacts = std::env::temp_dir().join(format!(
            "hermit-harness-import-root-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&artifacts).unwrap();
        let recorded = |summary: Option<serde_json::Value>| {
            let path = artifacts.join("summary.json");
            match summary {
                Some(summary) => fs::write(&path, summary.to_string()).unwrap(),
                None => fs::remove_file(&path).unwrap(),
            }
            super::recorded_import_root(&artifacts)
        };

        assert_eq!(recorded(Some(serde_json::json!({"cells": 1}))), Ok(None));
        assert_eq!(
            recorded(Some(
                serde_json::json!({"imported": {"root": "/srv/import", "missing_cells": []}})
            )),
            Ok(Some(std::path::PathBuf::from("/srv/import")))
        );
        for summary in [
            serde_json::json!({"imported": {"root": "relative/import"}}),
            serde_json::json!({"imported": {"missing_cells": []}}),
            serde_json::json!({"imported": null}),
        ] {
            let error = recorded(Some(summary.clone())).unwrap_err();
            assert!(error.contains("no absolute \"root\""), "{summary}: {error}");
        }
        for summary in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!(3),
            serde_json::json!("imported"),
        ] {
            let error = recorded(Some(summary.clone())).unwrap_err();
            assert!(error.contains("is not a JSON object"), "{summary}: {error}");
        }
        let error = recorded(None).unwrap_err();
        assert!(
            error.contains("whether the run imported its rows is unknown"),
            "{error}"
        );
        fs::write(artifacts.join("summary.json"), "{").unwrap();
        assert!(super::recorded_import_root(&artifacts).is_err());

        // The configuration reads each recorded verify-log directory from
        // where the index below the recorded root restored it.
        let import_root = artifacts.join("import");
        let index_dir = import_root.join(parity::IMPORTED_LOGS_DIR);
        fs::create_dir_all(&index_dir).unwrap();
        let entry = serde_json::json!({
            "schema": parity::IMPORTED_LOGS_SCHEMA,
            "verify_log_dir": "/tmp/cell/verify-logs",
            "restored": format!("{}/r1", parity::IMPORTED_LOGS_DIR),
            "reason": null,
            "route": "local",
            "container": "",
            "re_platform": "local",
        });
        fs::write(
            index_dir.join(parity::IMPORTED_LOGS_INDEX),
            format!("{entry}\n"),
        )
        .unwrap();
        let request = super::ParityCompareRequest {
            artifacts: artifacts.clone(),
            cells: Vec::new(),
            output: None,
            jobs: 1,
        };
        let configured = |summary: serde_json::Value| {
            fs::write(artifacts.join("summary.json"), summary.to_string()).unwrap();
            super::parity_compare_config(&artifacts, &request, "run", "sha")
        };
        let config = configured(serde_json::json!({
            "imported": {"root": import_root.to_str().unwrap()}
        }))
        .unwrap();
        let imported = config.imported_logs.as_ref().expect("the import's logs");
        assert_eq!(
            imported.restored("/tmp/cell/verify-logs"),
            Ok(index_dir.join("r1").as_path())
        );
        assert!(imported.restored("/tmp/other/verify-logs").is_err());
        let config = configured(serde_json::json!({"cells": 1})).unwrap();
        assert!(config.imported_logs.is_none());
        assert!(configured(serde_json::json!({"imported": {"root": "import"}})).is_err());
        fs::remove_dir_all(&artifacts).unwrap();
    }

    /// The `test-harness run` arguments of a committed DAG step up to its
    /// output paths, or `None` when the step runs no harness.
    fn harness_run_args(cmd: &str) -> Option<Vec<String>> {
        let (_, rest) = cmd.split_once("test-harness run ")?;
        let mut args = Vec::new();
        for word in rest.split_whitespace() {
            if matches!(word, "--results" | "--junit") {
                break;
            }
            args.push(word.trim_end_matches('\'').to_string());
            if word.ends_with('\'') {
                break;
            }
        }
        Some(args)
    }

    /// Every committed hosted-portable `test-harness run` command parses and
    /// validates as the harness itself reads it, and names each
    /// hosted-portable excluded backend exactly once; the local steps name
    /// none, except the portable-strict-compat-only compat step, which names
    /// exactly the backends its release build cannot run. `parse` exits the
    /// process on a repeated `--exclude-backend`, so the flag is counted
    /// before parsing to name the offending step.
    #[test]
    fn committed_hosted_portable_harness_commands_exclude_each_backend_once() {
        use hermit_manifest_plan::validation_dag::HOSTED_PORTABLE_EXCLUDED_BACKENDS;
        use hermit_manifest_plan::validation_dag::PORTABLE_FOCUSED_EXCLUDED_BACKENDS;
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let mut hosted = 0;
        let mut local = 0;
        let mut portable_focused = 0;
        for step in &committed.steps {
            let Some(values) = harness_run_args(&step.cmd) else {
                continue;
            };
            assert!(
                values
                    .iter()
                    .filter(|value| *value == "--exclude-backend")
                    .count()
                    <= HOSTED_PORTABLE_EXCLUDED_BACKENDS.len(),
                "{}: {values:?}",
                step.tag()
            );
            let args = parse(values.clone().into_iter());
            validate_args("run", &args);
            if step.labels == ["hosted-portable"] {
                hosted += 1;
                assert_eq!(
                    args.selection.exclude_backends,
                    HOSTED_PORTABLE_EXCLUDED_BACKENDS,
                    "{}: {values:?}",
                    step.tag()
                );
            } else if step.tag() == "portablecompat.manifest_compat" {
                portable_focused += 1;
                assert_eq!(
                    args.selection.exclude_backends,
                    PORTABLE_FOCUSED_EXCLUDED_BACKENDS,
                    "{}: {values:?}",
                    step.tag()
                );
            } else {
                local += 1;
                assert!(
                    args.selection.exclude_backends.is_empty(),
                    "{}: {values:?}",
                    step.tag()
                );
            }
        }
        // Thirteen hosted-portable manifest buckets, one step each: twelve
        // after backend-parity-c was folded into c-programs
        // (https://github.com/rrnewton/hermit/issues/3301), plus compat since
        // fold 1 of https://github.com/rrnewton/hermit/issues/3448.
        assert_eq!(hosted, 13, "hosted-portable harness steps");
        assert_eq!(portable_focused, 1, "portable-strict-compat-only steps");
        assert!(local > hosted, "local harness steps: {local}");
    }

    /// The parity post-pass must finish inside the dagrun step that runs the
    /// harness, which kills the step at its wall timeout. Every committed
    /// `test-harness run` step has a timeout of at least
    /// [`parity::PARITY_STEP_WALL_FLOOR`], the bound the post-pass counts
    /// from the step's start, and a step that runs the harness inside the
    /// pinned root forwards that start into it.
    #[test]
    fn every_harness_step_leaves_the_parity_post_pass_inside_its_wall_bound() {
        use hermit_manifest_plan::parity;
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let forwarded = format!(
            "--env {} ",
            dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV
        );
        let mut pinned = 0;
        let mut direct = 0;
        for step in committed
            .steps
            .iter()
            .filter(|step| harness_run_args(&step.cmd).is_some())
        {
            let timeout = dagrun::resolved_wall_timeout(step, committed.default_step_timeout, 1.0);
            assert!(
                u64::try_from(timeout).unwrap() >= parity::PARITY_STEP_WALL_FLOOR.as_secs(),
                "{}: {timeout} s is shorter than the parity post-pass's {} s step bound",
                step.tag(),
                parity::PARITY_STEP_WALL_FLOOR.as_secs()
            );
            assert!(!step.cmd.contains("env -i"), "{}: {}", step.tag(), step.cmd);
            if let Some((wrapper, _)) = step.cmd.split_once(" -- ") {
                if wrapper.contains("run-in-pinned-root.sh") {
                    assert!(wrapper.contains(&forwarded), "{}: {wrapper}", step.tag());
                    pinned += 1;
                    continue;
                }
            }
            assert!(
                !step.cmd.contains("run-in-pinned-root.sh"),
                "{}: {}",
                step.tag(),
                step.cmd
            );
            direct += 1;
        }
        // (18, 15) until slice S6 of https://github.com/rrnewton/hermit/issues/3301
        // folded e2e.manifest_backend_parity_c and its _on_host twin into the
        // c-programs pair; +2 pinned and +1 direct for the privileged
        // system-utils bucket that owns sysfs-sanitized-prefixes; +2 direct
        // for e2e.manifest_compat and its _on_host twin, which run on the host
        // because the corpus's programs are host-installed (fold 1 of
        // https://github.com/rrnewton/hermit/issues/3448).
        // +1 direct for portablecompat.manifest_compat, the corpus-only run
        // type's bucket, which also runs on the host, and +1 for
        // sabrecompat.manifest_compat, the SaBRe run type's host bucket, and
        // +1 for strictcompat.manifest_compat, the strict run type's, and +1
        // for rrcompat.manifest_compat, the rr run type's. +16 direct for
        // the full-buck-e2e import twins (<bucket>_buck), which import the
        // rows e2e.buck_cells wrote on the host and so run outside the root.
        // -1 direct when strictcompat.manifest_compat was retired with the
        // compat/strict-<row> tests (https://github.com/rrnewton/hermit/issues/4276),
        // and +1 for supercompat.manifest_compat, the super run type's host
        // bucket of the corpus's heavy rows (same issue).
        assert_eq!((pinned, direct), (19, 37));
    }

    /// The full profile's harness nodes together report each cell of the
    /// parity population exactly once: a node reports a cell when it plans
    /// either verify side, and exactly one node plans each side. Every
    /// population cell's ptrace side is selected by full validation, so no
    /// population cell goes unreported, whether or not its candidate runs.
    /// An `E2E_PARITY_SELECT` naming every applicable cell is reported the
    /// same way, apart from the cells no full node plans a side of, which
    /// every node drops with a warning.
    #[test]
    fn the_full_profile_reports_each_population_parity_cell_exactly_once() {
        use hermit_manifest_plan::parity;
        let root = super::root(None);
        let manifests = ManifestSet::load(&root).unwrap();
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let full = dagrun::select_steps_by_labels(&committed, &["full".into()])
            .expect("actual full selection");
        let matrix = parity::ParityMatrix::derive(&manifests).unwrap();
        let selection = matrix.population();
        let applicable = matrix
            .cells()
            .filter(|(_, availability)| availability.applicable())
            .map(|(cell, _)| cell.clone())
            .collect::<BTreeSet<_>>();
        let mut reported = BTreeMap::<parity::ParityCellId, Vec<String>>::new();
        let mut explicitly = BTreeMap::<parity::ParityCellId, Vec<String>>::new();
        let mut nodes = 0;
        for step in &full.steps {
            let Some(values) = harness_run_args(&step.cmd) else {
                continue;
            };
            nodes += 1;
            let args = parse(values.into_iter());
            validate_args("run", &args);
            let planned = planned_verify(&run_cells(&manifests, &args).unwrap());
            let (scope, warnings) = parity::post_pass_scope(&selection, &BTreeSet::new(), &planned);
            assert!(warnings.is_empty(), "{}: {warnings:?}", step.tag());
            for cell in scope {
                reported.entry(cell).or_default().push(step.tag());
            }
            let (scope, warnings) =
                parity::post_pass_scope(&BTreeSet::new(), &applicable, &planned);
            assert_eq!(
                warnings.len(),
                applicable.len() - scope.len(),
                "{}: each dropped cell is named",
                step.tag()
            );
            for cell in scope {
                explicitly.entry(cell).or_default().push(step.tag());
            }
        }
        // 15 until e2e.manifest_backend_parity_c was folded into
        // e2e.manifest_c_programs (slice S6 of
        // https://github.com/rrnewton/hermit/issues/3301); +1 for
        // privileged-e2e.manifest_system_utils. The selected cells are still
        // each reported once. Slice S13 added the DBT parity cells of
        // c-programs/cpuid-probe and c-programs/pid-probe, taking 192 to 194.
        // 16 since e2e.manifest_compat (fold 1 of
        // https://github.com/rrnewton/hermit/issues/3448); it plans no parity
        // cell, so the selected cells and their reports are unchanged.
        assert_eq!(nodes, 16);
        // The size of the population is not pinned here: parity.rs derives it
        // from the manifests and parity-cells.json snapshots it. This pins
        // what no generator checks: each population cell is reported by
        // exactly one full node.
        let lines = reported.values().map(Vec::len).sum::<usize>();
        assert_eq!(lines, selection.len());
        assert_eq!(reported.keys().cloned().collect::<BTreeSet<_>>(), selection);
        let duplicated = reported
            .iter()
            .filter(|(_, tags)| tags.len() > 1)
            .collect::<Vec<_>>();
        assert!(duplicated.is_empty(), "{duplicated:?}");
        let duplicated = explicitly
            .iter()
            .filter(|(_, tags)| tags.len() > 1)
            .collect::<Vec<_>>();
        assert!(duplicated.is_empty(), "{duplicated:?}");
        let unreported = applicable
            .iter()
            .filter(|cell| !explicitly.contains_key(*cell))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        // 4 applicable cells were unreported until fold 3 of
        // https://github.com/rrnewton/hermit/issues/3448 gave 212 compat rows a
        // SaBRe verify cell, which makes their SaBRe parity cell applicable.
        // e2e.manifest_compat plans the ptrace side of 185 of them. The other
        // 27 are the rows only the SaBRe run type runs, so no full node plans
        // either side of their cell; so are the 3 single-image rows
        // lua-direct, perl-direct and df-direct.
        // How many cells are applicable is the parity census's count, which
        // ci/compat-envelope/parity-cells.json records and a test keeps
        // current. This pins what the census does not show: which applicable
        // cells no full node reports.
        // 4 + 30 until the LiteInst reset
        // (https://github.com/rrnewton/hermit/issues/3745) switched off the
        // LiteInst cells of applications/example-timed-progress-bar and
        // c-programs/socket-timestamp-edge-cases. Both were enabled but not
        // run by full, and full planned neither side of their parity cell, so
        // they were unreported; switched off, they are no longer applicable.
        // The 2 left are example-timed-progress-bar's DBT and KVM cells.
        // 2 + 5 since https://github.com/rrnewton/hermit/issues/4269 moved 25
        // of the 30 SaBRe-run-type-only rows' ptrace cells into full: the 5
        // left are rustc, javac, java and node, whose ptrace cells are the
        // super suite's, and timeout, whose ptrace cell is unselected.
        assert_eq!(unreported.len(), 2 + 5, "{unreported:?}");
        let sabre_only = unreported
            .iter()
            .filter(|cell| cell.starts_with("compat/") && cell.ends_with("@sabre"))
            .count();
        assert_eq!(sabre_only, 5, "{unreported:?}");
    }

    #[test]
    fn generated_expected_plan_is_versioned_and_matches_the_tracked_file() {
        let root = super::root(None);
        let manifests = ManifestSet::load(&root).unwrap();
        let generated = expected_plan_document(&root, &manifests);
        assert_eq!(generated["schema"], EXPECTED_PLAN_SCHEMA);
        let tracked: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap())
                .unwrap();
        assert_eq!(tracked, generated);
    }

    /// A kvm cell is withheld where KVM is proven absent whatever its
    /// `requires` says, so the plan row a Buck generator or validate.rs routes
    /// from must name `kvm`; a row that omitted it would schedule the cell on
    /// a host that can only report it HOST-INAPPLICABLE.
    #[test]
    fn every_kvm_plan_row_requires_the_kvm_host_capability() {
        let root = super::root(None);
        let manifests = ManifestSet::load(&root).unwrap();
        let (_, rows) = super::required_plan_rows(&manifests);
        let capabilities = |row: &serde_json::Value| {
            row.get("requires_host_capabilities")
                .and_then(serde_json::Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .map(|value| value.as_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let kvm = rows
            .iter()
            .filter(|row| row["backend"] == "kvm")
            .collect::<Vec<_>>();
        // How many there are is the plan's to say; ci/expected-e2e-plan.json
        // records it and `validate` refuses a stale copy.
        assert!(!kvm.is_empty());
        let missing = kvm
            .iter()
            .filter(|row| !capabilities(row).contains(&"kvm".to_string()))
            .map(|row| format!("{}/{}@kvm", row["test"], row["mode"]))
            .collect::<Vec<_>>();
        assert!(missing.is_empty(), "{missing:?}");
        let elsewhere = rows
            .iter()
            .filter(|row| row["backend"] != "kvm")
            .filter(|row| capabilities(row).contains(&"kvm".to_string()))
            .count();
        assert_eq!(elsewhere, 0);
        // The cpuid-probe kvm row keeps its `requires` capability as well.
        let both = rows
            .iter()
            .filter(|row| capabilities(row) == ["cpuid-faulting", "kvm"])
            .map(|row| {
                (
                    row["test"].as_str().unwrap(),
                    row["backend"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(both, [("c-programs/cpuid-probe", "kvm")]);
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

    /// A terminal attempt row of one cell, with only the fields the
    /// structured report reads set; every other field takes its serde default.
    fn attempt_row(
        test: &str,
        attempt: u64,
        outcome: &str,
        failure_class: Option<&str>,
        classification: &str,
        relaxations: &[&str],
        reason: Option<&str>,
    ) -> CellResult {
        serde_json::from_value(serde_json::json!({
            "schema": 4, "run_id": "fixture", "hermit_sha": "sha", "source_tree_dirty": false,
            "test": test, "category": "fixture", "lane": "portable", "mode": "verify",
            "backend": "ptrace", "classification": classification, "outcome": outcome,
            "failure_class": failure_class, "attempt": attempt, "relaxations": relaxations,
            "reason": reason, "argv": [], "guest_argv": [], "env": {}, "cwd": "/repo",
            "shell_command": "", "attempts": [], "artifact_dir": "/repo/a",
        }))
        .unwrap()
    }

    #[test]
    fn structured_test_results_are_machine_readable_and_exact_on_failure() {
        use hermit_manifest_plan::runner::ObservedResult;
        let diagnostic = "diagnostic (a product failure does not fail the run): bounded probe";
        let histories = vec![
            vec![attempt_row(
                "t/pass",
                1,
                "PASS",
                None,
                "required",
                &[],
                None,
            )],
            vec![
                attempt_row(
                    "t/recovers",
                    1,
                    "FAIL",
                    Some("product_failure"),
                    "required",
                    &[],
                    Some("diverged at rec 7"),
                ),
                attempt_row("t/recovers", 2, "PASS", None, "required", &[], None),
            ],
            vec![attempt_row(
                "t/fails",
                1,
                "FAIL",
                Some("product_failure"),
                "required",
                &[],
                Some("exit 3"),
            )],
            vec![attempt_row(
                "t/noresult",
                1,
                "ERROR",
                Some("no_result"),
                "required",
                &[],
                Some("empty run 1"),
            )],
            vec![attempt_row(
                "t/diag",
                1,
                "FAIL",
                Some("product_failure"),
                "diagnostic",
                &[diagnostic],
                Some("timed out"),
            )],
            // A diagnostic cell that could not produce a product verdict still blocks.
            vec![attempt_row(
                "t/diag-error",
                1,
                "ERROR",
                Some("understood_infrastructure_failure"),
                "diagnostic",
                &[diagnostic],
                Some("launch refused"),
            )],
            vec![attempt_row(
                "t/skipped",
                1,
                "HOST-INAPPLICABLE",
                None,
                "required",
                &[],
                Some("no kvm"),
            )],
            // A diagnostic cell that exceeded its own budget is a measured failure.
            vec![{
                let mut row = attempt_row(
                    "t/diag-timeout",
                    1,
                    "FAIL",
                    Some("no_result"),
                    "diagnostic",
                    &[diagnostic],
                    Some("cell exceeded 20 wall s backstop (20 s CPU budget)"),
                );
                row.error_kind = Some("wall-timeout".into());
                row.result = Some(ObservedResult::Timeout);
                row
            }],
            // A retried diagnostic product failure that ends without a product verdict blocks.
            vec![
                attempt_row(
                    "t/diag-retry",
                    1,
                    "FAIL",
                    Some("product_failure"),
                    "diagnostic",
                    &[diagnostic],
                    Some("diverged"),
                ),
                attempt_row(
                    "t/diag-retry",
                    2,
                    "ERROR",
                    Some("no_result"),
                    "diagnostic",
                    &[diagnostic],
                    Some("empty run 2"),
                ),
            ],
        ];
        let report = structured_test_results(&histories).unwrap();
        let wire: serde_json::Value =
            serde_json::from_slice(&report.to_diagnostic_json().unwrap()).unwrap();
        assert_eq!(wire["schema"], 4);
        assert_eq!(
            wire["executed_tests"], 8,
            "the host-inapplicable cell executed nothing"
        );
        let rows = wire["results"].as_array().unwrap();
        let summary: Vec<(String, String, Vec<String>)> = rows
            .iter()
            .map(|row| {
                (
                    row["id"].as_str().unwrap().to_string(),
                    row["result"].as_str().unwrap().to_string(),
                    row["attempt_results"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|attempt| attempt["outcome"].as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect();
        let s = |value: &str| value.to_string();
        assert_eq!(
            summary,
            vec![
                (s("t/pass [ptrace/verify]"), s("pass"), vec![s("passed")]),
                (
                    s("t/recovers [ptrace/verify]"),
                    s("pass"),
                    vec![s("failed"), s("passed")]
                ),
                (s("t/fails [ptrace/verify]"), s("fail"), vec![s("failed")]),
                (
                    s("t/noresult [ptrace/verify]"),
                    s("fail"),
                    vec![s("no_result")]
                ),
                (
                    s("t/diag [ptrace/verify]"),
                    s("diagnostic_fail"),
                    vec![s("failed")]
                ),
                (
                    s("t/diag-error [ptrace/verify]"),
                    s("fail"),
                    vec![s("infrastructure_error")]
                ),
                (
                    s("t/diag-timeout [ptrace/verify]"),
                    s("diagnostic_fail"),
                    vec![s("wall_timeout")]
                ),
                (
                    s("t/diag-retry [ptrace/verify]"),
                    s("fail"),
                    vec![s("failed"), s("no_result")]
                ),
            ]
        );
        assert_eq!(rows[4]["diagnostic_reason"], "bounded probe");
        assert_eq!(rows[4]["attempt_results"][0]["detail"], "timed out");
        assert_eq!(rows[1]["attempt_results"][0]["detail"], "diverged at rec 7");
        assert!(
            rows.iter()
                .enumerate()
                .all(|(index, row)| matches!(index, 4 | 6) != row["diagnostic_reason"].is_null())
        );
        // The report dagrun reads back has exactly one diagnostic and three blocking failures.
        let parsed = TestResults::from_declared_schema_json_slice(
            &report.to_diagnostic_json().unwrap(),
            dagrun::DIAGNOSTIC_RESULTS_SCHEMA,
        )
        .unwrap();
        let parsed = parsed.results.unwrap();
        assert_eq!(
            parsed
                .iter()
                .filter(|row| row.is_diagnostic_failure())
                .count(),
            2
        );
        assert_eq!(
            parsed
                .iter()
                .filter(|row| row.is_blocking_failure())
                .count(),
            4
        );
        // A cell the harness reports as an ERROR (its history could not be
        // summarized) is never excused, even if its last attempt was a
        // diagnostic product FAIL.
        let diagnostic_history = &histories[4];
        let mut summarized_error = diagnostic_history[0].clone();
        summarized_error.outcome = "ERROR".into();
        assert_eq!(
            excused_diagnostic(true, &summarized_error, diagnostic_history),
            None
        );
        assert_eq!(
            excused_diagnostic(true, &diagnostic_history[0], diagnostic_history),
            Some("bounded probe")
        );
        // A run that did not declare --diagnostic-results excuses nothing.
        assert_eq!(
            excused_diagnostic(false, &diagnostic_history[0], diagnostic_history),
            None
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
        let (capabilities, reason) =
            host_inapplicable_reason(&requires, Some("ptrace"), &absent, None).unwrap();
        assert_eq!(capabilities, ["cpuid-faulting"]);
        assert!(reason.contains("NOT RUN, NOT a pass, no coverage"));
        assert!(reason.contains("planted absence"));

        let undeclared = vec!["linux".to_string(), "ptrace".to_string()];
        assert!(host_inapplicable_reason(&undeclared, Some("ptrace"), &absent, None).is_none());

        let present = BTreeMap::from([(
            HostCapability::CpuidFaulting,
            HostCapabilityVerdict {
                present: true,
                evidence: "planted presence".into(),
            },
        )]);
        assert!(host_inapplicable_reason(&requires, Some("ptrace"), &present, None).is_none());
    }

    /// A kvm cell needs KVM whatever its `requires` say (the kvm backend cells do
    /// not carry the `kvm` token), so proven absence withholds it; other
    /// backends and a present device run.
    #[test]
    fn a_kvm_cell_is_withheld_where_kvm_is_proven_absent() {
        let verdicts = |present| {
            BTreeMap::from([(
                HostCapability::Kvm,
                HostCapabilityVerdict {
                    present,
                    evidence: "open(/dev/kvm, O_RDWR) = -1 errno=2".into(),
                },
            )])
        };
        let requires = vec![
            "linux".to_string(),
            "x86_64".to_string(),
            "ptrace".to_string(),
        ];
        let (capabilities, reason) =
            host_inapplicable_reason(&requires, Some("kvm"), &verdicts(false), None).unwrap();
        assert_eq!(capabilities, ["kvm"]);
        assert!(reason.contains("errno=2"), "{reason}");
        assert!(
            host_inapplicable_reason(&requires, Some("ptrace"), &verdicts(false), None).is_none()
        );
        assert!(host_inapplicable_reason(&requires, None, &verdicts(false), None).is_none());
        assert!(host_inapplicable_reason(&requires, Some("kvm"), &verdicts(true), None).is_none());
    }

    /// The in-guest LiteInst runtime, behind both the liteinst and in-guest-trap
    /// backends, needs CPUID faulting whatever a cell's `requires` say, so
    /// proven absence withholds those cells and no other backend's. Their
    /// required plan rows name the capability too, which is what keeps the Buck
    /// cell generator from sending them to remote execution workers, where CPUID
    /// faulting may be absent (a validate of d60c6e78 sent 251 cells of each
    /// backend there and every one that landed on such a worker failed).
    #[test]
    fn an_in_guest_liteinst_cell_needs_cpuid_faulting() {
        let verdicts = |present| {
            BTreeMap::from([(
                HostCapability::CpuidFaulting,
                HostCapabilityVerdict {
                    present,
                    evidence: "arch_prctl(ARCH_SET_CPUID, 0) = -1 errno=19".into(),
                },
            )])
        };
        let requires = vec!["linux".to_string(), "x86_64".to_string()];
        for backend in ["liteinst", "in-guest-trap"] {
            let (capabilities, reason) =
                host_inapplicable_reason(&requires, Some(backend), &verdicts(false), None)
                    .unwrap_or_else(|| panic!("{backend} ran without CPUID faulting"));
            assert_eq!(capabilities, ["cpuid-faulting"], "{backend}");
            assert!(reason.contains("errno=19"), "{reason}");
            assert!(
                host_inapplicable_reason(&requires, Some(backend), &verdicts(true), None).is_none(),
                "{backend}"
            );
        }
        for backend in ["ptrace", "kvm", "dbt", "sabre", "e9patch"] {
            assert!(
                host_inapplicable_reason(&requires, Some(backend), &verdicts(false), None)
                    .is_none(),
                "{backend}"
            );
        }

        let (_, rows) = required_plan_rows(&ManifestSet::load(&super::root(None)).unwrap());
        let mut in_guest = 0;
        for row in &rows {
            let backend = row["backend"].as_str().unwrap();
            let capabilities = row
                .get("requires_host_capabilities")
                .and_then(JsonValue::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(JsonValue::as_str)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if matches!(backend, "liteinst" | "in-guest-trap") {
                in_guest += 1;
                assert!(capabilities.contains(&"cpuid-faulting"), "{row}");
            }
        }
        assert!(
            in_guest > 0,
            "the required plan has no in-guest LiteInst cell"
        );
    }

    /// Where the hermit binary reports an inexact branch counter it refuses
    /// `hermit run --strict` and `hermit record start --strict` on ptrace and
    /// e9patch, so exactly the cells that run those are withheld, with the
    /// binary's evidence. A replay cell runs `record start --strict`
    /// (https://github.com/rrnewton/hermit/issues/3810). A custom cell without
    /// `--strict` runs plain `run`, which the binary runs, so it still runs
    /// here. An unprobed binary withholds nothing.
    #[test]
    fn strict_run_pmu_clock_cells_are_withheld_where_the_binary_reports_an_inexact_counter() {
        let manifests = ManifestSet::load(&super::root(None)).unwrap();
        let cell = |test: &str, mode: &str| {
            manifests
                .select(&hermit_manifest_plan::runner::Selection {
                    test: Some(test.into()),
                    mode: Some(mode.into()),
                    backend: Some("ptrace".into()),
                    population: Some(hermit_manifest_plan::runner::Population::Required),
                    ..hermit_manifest_plan::runner::Selection::default()
                })
                .unwrap()
                .remove(0)
        };
        let present = BTreeMap::from([(
            HostCapability::Kvm,
            HostCapabilityVerdict {
                present: true,
                evidence: "planted presence".into(),
            },
        )]);
        let counter = |present| HostCapabilityVerdict {
            present,
            evidence: "CPU fixture: Reverie performance-counter validation failed".into(),
        };
        let withheld = |cell: &SelectedCell| {
            cell_host_inapplicable_reason(cell, &present, Some(&counter(false)))
        };

        let strict_runs = [
            cell("c-programs/random-readv-stream", "verify"),
            cell("c-programs/ipc-determinism", "chaos"),
            cell("c-programs/environment-and-workdir", "custom"),
            cell("c-programs/random-readv-stream", "replay"),
        ];
        for mut strict_run in strict_runs {
            for backend in ["ptrace", "e9patch"] {
                strict_run.id.backend = Some(backend.into());
                let (capabilities, reason) = withheld(&strict_run).unwrap();
                assert_eq!(capabilities, ["exact-branch-counter"]);
                assert!(
                    reason.contains("NOT RUN, NOT a pass, no coverage"),
                    "{reason}"
                );
                assert!(reason.contains("validation failed"), "{reason}");
                assert!(
                    cell_host_inapplicable_reason(&strict_run, &present, Some(&counter(true)))
                        .is_none()
                );
                assert!(cell_host_inapplicable_reason(&strict_run, &present, None).is_none());
            }
            for backend in [
                Some("kvm"),
                Some("dbt"),
                Some("sabre"),
                Some("liteinst"),
                None,
            ] {
                strict_run.id.backend = backend.map(Into::into);
                assert!(withheld(&strict_run).is_none(), "{:?}", strict_run.id);
            }
        }

        let custom = cell("system-utils/clock-determinism", "custom");
        assert!(
            !custom.test.modes["custom"]
                .args
                .iter()
                .any(|arg| arg == "--strict")
        );
        // `--namespace-only` arms no branch counter, so the binary does not
        // refuse it for the counter even with `--strict` (with the harness's
        // explicit `--backend` it is a usage error, which must fail visibly).
        let mut namespace_only = cell("c-programs/environment-and-workdir", "custom");
        for flag in ["--namespace-only", "--lite"] {
            let args = &mut namespace_only.test.modes.get_mut("custom").unwrap().args;
            args.retain(|arg| arg != "--namespace-only" && arg != "--lite");
            args.push(flag.into());
            assert!(withheld(&namespace_only).is_none(), "{flag}");
        }
        assert!(withheld(&custom).is_none(), "{:?}", custom.id);
    }

    const GUARDED_WORKFLOW: &str = r#"    # --allow-cgroup-failure is documented here but not executed.
        if [[ ${GITHUB_ACTIONS:-} != true ]]; then
          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2
          exit 2
        fi
        timeout 720s ci/run-dag.sh privileged --unsafe-no-cgroups
"#;

    #[test]
    fn workflow_outer_bounds_cover_constructed_graphs_and_setup() {
        let root = super::root(None);
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let portable = dagrun::select_steps_by_labels(&committed, &["hosted-portable".into()])
            .expect("actual hosted portable selection");
        let shards: serde_json::Value =
            serde_json::from_str(include_str!("../../../portable-shards.json")).unwrap();
        let mut portable_workflow =
            super::parse_yaml(&root.join(".github/workflows/ci-portable.yml"))
                .expect("portable workflow");
        super::audit_portable_preflight_budget(&portable_workflow, &portable, &shards).unwrap();
        super::audit_portable_checks_budget(&portable_workflow, &portable, &shards).unwrap();
        super::audit_portable_reducer_prepared_tools(&portable_workflow).unwrap();
        portable_workflow["jobs"]["preflight"]["timeout-minutes"] =
            serde_yaml::to_value(10_u64).unwrap();
        let error = super::audit_portable_preflight_budget(&portable_workflow, &portable, &shards)
            .unwrap_err();
        assert!(
            error.contains(
                "portable preflight job 600s must cover its 4200s constructed DAG critical path plus at least 420s"
            ),
            "{error}"
        );
        portable_workflow["jobs"]["checks"]["timeout-minutes"] =
            serde_yaml::to_value(49_u64).unwrap();
        let error = super::audit_portable_checks_budget(&portable_workflow, &portable, &shards)
            .unwrap_err();
        assert!(
            error.contains(
                "portable checks job 2940s must cover its 2400s constructed DAG critical path plus at least 600s"
            ),
            "{error}"
        );
        // The check job's public selector check.script_unit_tests resolves to
        // its hosted twin, whose own wall the audit reads; an unknown selector
        // still refuses.
        let mut longer_twin = portable.clone();
        longer_twin
            .steps
            .iter_mut()
            .find(|step| step.tag() == "check.script_unit_tests_on_host")
            .unwrap()
            .timeout = 2401;
        let error = super::audit_portable_checks_budget(&portable_workflow, &longer_twin, &shards)
            .unwrap_err();
        assert!(
            error.contains("portable checks critical path changed from 2400s to 2401s"),
            "{error}"
        );
        let mut unknown = shards.clone();
        unknown["check_nodes"]
            .as_array_mut()
            .unwrap()
            .push("check.no_such_node".into());
        let error = super::audit_portable_checks_budget(&portable_workflow, &portable, &unknown)
            .unwrap_err();
        assert!(
            error.contains("cannot select portable checks closure"),
            "{error}"
        );

        let mut missing_reducer_tools = portable_workflow.clone();
        missing_reducer_tools["jobs"]["regular"]["steps"]
            .as_sequence_mut()
            .unwrap()
            .retain(|step| {
                step["name"].as_str()
                    != Some("Download reducer manifest plan tools and rust-script binaries")
            });
        let error =
            super::audit_portable_reducer_prepared_tools(&missing_reducer_tools).unwrap_err();
        assert!(error.contains("prepared rust-script artifact"), "{error}");

        let mut privileged_workflow =
            super::parse_yaml(&root.join(".github/workflows/ci-privileged.yml"))
                .expect("privileged workflow");
        super::audit_privileged_workflow_overhead(&privileged_workflow).unwrap();
        privileged_workflow["jobs"]["privileged"]["timeout-minutes"] =
            serde_yaml::to_value(44_u64).unwrap();
        let error = super::audit_privileged_workflow_overhead(&privileged_workflow).unwrap_err();
        assert!(
            error.contains(
                "privileged job 2640s must cover 2910s of explicit inner step budgets plus at least 300s"
            ),
            "{error}"
        );
    }

    #[test]
    fn portable_shard_budgets_resolve_actual_hosted_nodes_without_losing_checks() {
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let hosted = dagrun::select_steps_by_labels(&committed, &["hosted-portable".into()])
            .expect("actual hosted selection");
        let mut steps = hosted
            .steps
            .iter()
            .map(|step| (step.tag(), step))
            .collect::<BTreeMap<_, _>>();
        let shards: serde_json::Value =
            serde_json::from_str(include_str!("../../../portable-shards.json")).unwrap();
        let expected_aliases = [
            // A hosted twin since the one-build change of 2026-09-30 moved
            // the local check into the pinned root.
            "check.dbt_runtime_abi",
            "doc.doctests",
            "doc.rustdoc",
            "lint.clippy",
            "test.hermit_unit",
            "test.detcore_unit",
            "test.detcore_misc",
            "test.detcore_parallel",
            "test.detcore_time",
            "test.regular_crates",
            "test.hermit_integration",
            "test.arbitrary_binaries",
            "test.applications_e2e",
            "test.app_strict_verify",
            "test.command_strict_verify",
            "test.ignored_syscall_regressions",
            "test.envelope_levels",
            "test.rr_suite_contract",
            "test.sabre_examples",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
        let mut actual_aliases = std::collections::BTreeSet::new();
        let mut resolved = std::collections::BTreeSet::new();
        let mut physical_rows = 0;
        for key in ["debug_shards", "release_shards"] {
            for shard in shards[key].as_array().unwrap() {
                for public in shard["nodes"].as_array().unwrap() {
                    let public = public.as_str().unwrap();
                    let step = super::portable_shard_step(&steps, public).unwrap();
                    let expected = if expected_aliases.contains(public) {
                        assert!(
                            !steps.contains_key(public),
                            "must exercise actual renamed node"
                        );
                        actual_aliases.insert(public);
                        format!("{public}_on_host")
                    } else {
                        public.to_string()
                    };
                    assert_eq!(step.tag(), expected);
                    assert!(std::ptr::eq(step, steps[&expected]));
                    assert!(
                        resolved.insert(step.tag()),
                        "duplicate physical shard target"
                    );
                    physical_rows += 1;
                }
            }
        }
        // No manifest node belongs in a test shard while it selects portable
        // cells: only the e2e jobs pack the parity-v1 archive the reducer reads
        // (ci/check-shard-coverage.sh enforces that against the committed cell
        // plan). shared-futex-c and util-c left the integration shard once
        // https://github.com/rrnewton/hermit/pull/3213 gave each a portable cell
        // (25 to 23). In slice S13 of
        // https://github.com/rrnewton/hermit/issues/3301, test.dbt_parity left
        // the dbt-parity shard (23 to 22; the shard is now dbt-runtime-abi) and
        // check.backend_parity_suites left the integration shard when
        // tests/backend-parity was retired (22 to 21). test.detcore_time joined
        // the unit shard when it was enrolled (21 to 22). test.liteinst_strict
        // left with its liteinst shard when LiteInst became in-guest only
        // (https://github.com/rrnewton/hermit/issues/3520; 22 to 21).
        assert_eq!(physical_rows, 21);
        assert_eq!(resolved.len(), 21);
        assert_eq!(actual_aliases, expected_aliases);
        // Run the complete real budget audit too: all original workflow,
        // critical-path and exact inversion-baseline comparisons remain active.
        super::audit_budget_ordering(&super::root(None)).unwrap();

        let hosted_name = "test.hermit_unit_on_host";
        let host = *steps.get(hosted_name).unwrap();
        assert_eq!(host.timeout, 900);
        assert!(std::ptr::eq(
            super::portable_shard_step(&steps, hosted_name).unwrap(),
            host
        ));
        let mut exact = (*host).clone();
        exact.job = "hermit_unit".into();
        exact.timeout = 17;
        let mut exact_steps = steps.clone();
        exact_steps.insert("test.hermit_unit".into(), &exact);
        let selected = super::portable_shard_step(&exact_steps, "test.hermit_unit").unwrap();
        assert!(
            std::ptr::eq(selected, &exact),
            "exact selector must win over hosted twin"
        );
        assert_eq!(
            selected.timeout, 17,
            "use actual selected budget, never a public-name default"
        );
        assert!(steps.remove(hosted_name).is_some());
        for unknown in ["test.hermit_unit", hosted_name, "test.no_such_shard_node"] {
            assert_eq!(
                super::portable_shard_step(&steps, unknown).unwrap_err(),
                format!("portable shard names missing DAG node {unknown}")
            );
        }
    }

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

    /// Runs the committed validation-levels privileged DAG step through the
    /// real public launcher, against a runner that records only its arguments
    /// and the process-group CPU scan marker it inherits. Under
    /// `--allow-cgroup-failure` the pinned runner reads each of these
    /// variables before it settles on running unboxed, so a step whose
    /// environment carries any of them keeps that boxing-capable launch, which
    /// never carries the marker. Only an Actions step carrying none of them,
    /// where that launch was certainly unboxed, selects `--unsafe-no-cgroups`,
    /// and only that launch hands its cells the marker. Outside Actions the
    /// step refuses before launching anything.
    #[test]
    fn validation_levels_privileged_dag_keeps_available_boxing_and_marks_only_the_unboxed_launch() {
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;
        use std::process::Command;

        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        const MARKER: &str = "HERMIT_E2E_ALLOW_PROCESS_GROUP_CPU_SCAN";
        const BOXING_INPUTS: [(&str, &str); 6] = [
            (
                "DAGRUN_DELEGATED_CGROUP",
                "/sys/fs/cgroup/delegated-by-an-outer-scheduler",
            ),
            ("DAGRUN_DELEGATED_UNBOXED", "1"),
            ("DAGRUN_IN_SCOPE", "1"),
            ("DAGRUN_SCOPE_UNIT", "dagrun-outer.scope"),
            ("DAGRUN_DIRECT_CGROUP", "1"),
            ("DAGRUN_FORCE_SCOPE_ATTEMPT", "1"),
        ];

        let workflow: YamlValue = serde_yaml::from_str(include_str!(
            "../../../../.github/workflows/validation-levels.yml"
        ))
        .unwrap();
        let step_script = workflow["jobs"]["full"]["steps"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|step| step["name"].as_str() == Some("Privileged PMU and CPUID test DAG"))
            .and_then(|step| step["run"].as_str())
            .expect("the validation-levels full job runs the privileged DAG step")
            .to_string();

        let scratch = Scratch(std::env::temp_dir().join(format!(
            "hermit-validation-levels-privileged-dag-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        )));
        let fixture = scratch.0.clone();
        let _ = fs::remove_dir_all(&fixture);
        for directory in ["ci", "agent-utils/common/bin", "bin", "runner-temp"] {
            fs::create_dir_all(fixture.join(directory)).unwrap();
        }
        let repo = super::root(None);
        for relative in ["ci/run-dag.sh", "ci/configure-build-jobs.sh"] {
            fs::copy(repo.join(relative), fixture.join(relative)).unwrap();
        }
        let write_executable = |path: PathBuf, body: String| {
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        // The launcher looks here when DAGRUN_BIN is unset, which the step
        // guarantees with `env -u DAGRUN_BIN`.
        write_executable(
            fixture.join("agent-utils/common/bin/dagrun"),
            format!(
                "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" \"${{{MARKER}-unset}}\" >\"$VALIDATION_LEVELS_DAG_CAPTURE\"\n"
            ),
        );
        // The launcher only resolves rust-script; nothing here executes it.
        write_executable(
            fixture.join("bin/rust-script"),
            "#!/bin/sh\nexit 97\n".into(),
        );
        let script = fixture.join("privileged-dag-step.sh");
        fs::write(&script, &step_script).unwrap();
        let capture = fixture.join("captured-launch");
        let path = format!(
            "{}:{}",
            fixture.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let run_step = |actions: bool, inputs: &[(&str, &str)], caller_marker: Option<&str>| {
            match fs::remove_file(&capture) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot reset the captured launch: {error}"),
            }
            let mut command = Command::new("bash");
            command
                .arg("-e")
                .arg(&script)
                .current_dir(&fixture)
                .env("PATH", &path)
                .env("RUNNER_TEMP", fixture.join("runner-temp"))
                .env("VALIDATION_LEVELS_DAG_CAPTURE", &capture)
                .env(
                    "DAGRUN_BIN",
                    fixture.join("a-caller-runner-the-step-must-drop"),
                )
                .env_remove("CI_DAG_BUILD_JOBS")
                .env_remove("RUN_DAG_FILE_OVERRIDE")
                .env_remove("VALIDATE_RUN_STATE")
                .env_remove("E2E_RESULT_ROOT")
                .env_remove("E2E_BUILD_ROOT");
            for (name, _) in BOXING_INPUTS {
                command.env_remove(name);
            }
            for (name, value) in inputs {
                command.env(name, value);
            }
            if actions {
                command.env("GITHUB_ACTIONS", "true");
            } else {
                command.env_remove("GITHUB_ACTIONS");
            }
            match caller_marker {
                Some(value) => command.env(MARKER, value),
                None => command.env_remove(MARKER),
            };
            let output = command.output().unwrap();
            let launch = match fs::read_to_string(&capture) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("cannot read the captured launch: {error}"),
            };
            (output, launch)
        };
        let launched = |label: &str,
                        (output, launch): (std::process::Output, Option<String>)|
         -> (String, String) {
            assert!(
                output.status.success(),
                "{label}: the step failed with {}: {}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let launch =
                launch.unwrap_or_else(|| panic!("{label}: the step never reached the runner"));
            let mut lines = launch.lines();
            let arguments = lines.next().unwrap_or_default().to_string();
            let marker = lines.next().unwrap_or_default().to_string();
            assert!(
                arguments.starts_with("run --dag ")
                    && arguments.contains(" --labels hosted-privileged -j 2 ")
                    && arguments.ends_with(" -v"),
                "{label}: unexpected runner arguments {arguments:?}"
            );
            (arguments, marker)
        };

        // Actions with no boxing input: the old launch was certainly unboxed,
        // so the step opts out by flag and its cells get the scan marker.
        for caller_marker in [None, Some("0")] {
            let label = format!("unboxed Actions step, caller marker {caller_marker:?}");
            let (arguments, marker) = launched(&label, run_step(true, &[], caller_marker));
            assert!(
                arguments.contains(" --unsafe-no-cgroups ")
                    && !arguments.contains("--allow-cgroup-failure"),
                "{label}: expected the explicit unboxed launch, got {arguments:?}"
            );
            assert_eq!(
                marker, "1",
                "{label}: the unboxed launch must hand its cells the marker"
            );
        }

        // Any boxing input keeps the launch that lets the runner box the DAG,
        // and that launch never carries the marker, even a caller's.
        for input in BOXING_INPUTS {
            for caller_marker in [None, Some("1")] {
                let label = format!(
                    "Actions step with {}, caller marker {caller_marker:?}",
                    input.0
                );
                let (arguments, marker) = launched(&label, run_step(true, &[input], caller_marker));
                assert!(
                    arguments.contains(" --allow-cgroup-failure ")
                        && !arguments.contains("--unsafe-no-cgroups"),
                    "{label}: available boxing was bypassed: {arguments:?}"
                );
                assert_eq!(
                    marker, "unset",
                    "{label}: a launch that may be boxed handed its cells the scan marker"
                );
            }
        }

        // Outside Actions the step refuses, whatever the environment says.
        for inputs in [&[][..], &BOXING_INPUTS[..1]] {
            let (output, launch) = run_step(false, inputs, Some("1"));
            assert_eq!(
                output.status.code(),
                Some(2),
                "outside Actions with {inputs:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(
                    "privileged DAG: refusing explicit unboxed execution outside GitHub Actions"
                ),
                "outside Actions with {inputs:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                launch, None,
                "outside Actions with {inputs:?}: the runner was launched"
            );
        }
    }

    /// The exact-command audit pins both launches of the validation-levels
    /// privileged DAG step: dropping either one is a different command.
    #[test]
    fn validation_levels_run_dag_audit_pins_both_privileged_launches() {
        let mut workflow: YamlValue = serde_yaml::from_str(include_str!(
            "../../../../.github/workflows/validation-levels.yml"
        ))
        .unwrap();
        const LABEL: &str = ".github/workflows/validation-levels.yml";
        audit_run_dag_workflow_runner(LABEL, &workflow).unwrap();
        let index = workflow["jobs"]["full"]["steps"]
            .as_sequence()
            .unwrap()
            .iter()
            .position(|step| step["name"].as_str() == Some("Privileged PMU and CPUID test DAG"))
            .expect("the validation-levels full job runs the privileged DAG step");
        let committed = workflow["jobs"]["full"]["steps"][index]["run"]
            .as_str()
            .unwrap()
            .to_string();
        let guard = "if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n  echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n  exit 2\nfi\n";
        let launch = |flag: &str| {
            format!(
                "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 {flag} --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v\n"
            )
        };
        for single in [
            format!("{guard}{}", launch("--unsafe-no-cgroups")),
            format!("{guard}{}", launch("--allow-cgroup-failure")),
        ] {
            assert_ne!(single, committed);
            workflow["jobs"]["full"]["steps"][index]["run"] = YamlValue::String(single.clone());
            let error = audit_run_dag_workflow_runner(LABEL, &workflow)
                .expect_err("a single-launch privileged step must not pass the exact audit");
            assert!(
                error.contains("differ from the exact Rust-runner commands"),
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
            "timeout --foreground --kill-after=10s 2160s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -v",
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
            "timeout --foreground --kill-after=10s 2160s ci/run-dag.sh privileged -v",
            "timeout --foreground --kill-after=10s 2160s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -v",
        ] {
            assert_eq!(command_timeout_seconds(command).unwrap(), Some(2160));
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
    fn validation_audits_share_the_aggregate_cpu_cap_serially() {
        assert_eq!(
            validation_audit_worker_capacity(true, Some("1")).configured(),
            1
        );
        assert_eq!(
            validation_audit_worker_capacity(true, None).configured(),
            DEFAULT_VALIDATE_AUDIT_JOBS,
            "an unaffected prebuilt gate must retain its existing two-worker schedule"
        );
        assert_eq!(
            validation_audit_worker_capacity(true, Some("2")).configured(),
            DEFAULT_VALIDATE_AUDIT_JOBS
        );
        assert_eq!(
            validation_audit_worker_capacity(true, Some("32")).configured(),
            DEFAULT_VALIDATE_AUDIT_JOBS,
            "an oversized explicit width must remain clamped to two audit workers"
        );
        for malformed in [Some("0"), Some("not-a-width")] {
            assert_eq!(
                validation_audit_worker_capacity(true, malformed).configured(),
                1,
                "an invalid explicit audit width must fail closed to serial execution"
            );
        }
        assert_eq!(
            validation_audit_worker_capacity(false, Some("2")).configured(),
            1,
            "audits without immutable prebuilt scripts retain serial execution"
        );
    }

    fn completed_audit_output_survives_an_unfinished_peer() {
        use std::io::Write;
        use std::path::PathBuf;

        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        struct ReleaseOnFlush {
            bytes: Vec<u8>,
            release: PathBuf,
        }
        impl Write for ReleaseOnFlush {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                if self
                    .bytes
                    .windows(b"second stdout\n".len())
                    .any(|part| part == b"second stdout\n")
                {
                    fs::write(&self.release, b"completed peer diagnostics observed")?;
                }
                Ok(())
            }
        }

        let path = std::env::temp_dir().join(format!(
            "hermit-audit-output-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir(&path).unwrap();
        let fixture = Scratch(path);
        let release = fixture.0.join("release");
        let mut stdout = ReleaseOnFlush {
            bytes: Vec::new(),
            release: release.clone(),
        };
        let mut stderr = Vec::new();
        // The first audit cannot succeed until its parent's output writer has
        // flushed the second audit. The old all-join buffering hits exit 97.
        // A fixed polling limit bounds this opponent even when publication fails.
        let blocked = "i=0; while [ ! -f \"$1\" ]; do i=$((i+1)); [ \"$i\" -lt 200 ] || exit 97; sleep 0.01; done; printf 'first stdout\\n'";
        let audits = [
            (
                PathBuf::from("/bin/sh"),
                vec!["-c", blocked, "audit", release.to_str().unwrap()],
            ),
            (
                PathBuf::from("/bin/sh"),
                vec![
                    "-c",
                    "printf 'second stdout\\n'; printf 'second stderr\\n' >&2; exit 23",
                ],
            ),
        ];
        let results =
            super::collect_audit_results(&fixture.0, &audits, 2, &mut stdout, &mut stderr).unwrap();
        assert_eq!(stdout.bytes, b"second stdout\nfirst stdout\n");
        let diagnostics = String::from_utf8(stderr).unwrap();
        assert_eq!(
            diagnostics
                .lines()
                .filter(|line| *line == "second stderr")
                .count(),
            1
        );
        assert!(diagnostics.contains("audit 1/2 START /bin/sh"));
        assert!(diagnostics.contains("audit 2/2 START /bin/sh"));
        assert!(diagnostics.contains("exit status: 23; elapsed="));
        assert!(
            diagnostics.find("audit 2/2 END").unwrap() < diagnostics.find("audit 1/2 END").unwrap()
        );
        // Retained terminal records stay in declared order; early publication
        // must neither reorder the final summary nor relabel the failing child.
        assert_eq!(results.len(), 2);
        assert!(results[0].status.as_ref().unwrap().success());
        assert_eq!(results[1].status.as_ref().unwrap().code(), Some(23));
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
        completed_audit_output_survives_an_unfinished_peer();
    }
    #[test]
    fn retry_policy_retries_only_classified_product_failures() {
        assert!(cell_result_is_retryable(
            "FAIL",
            Some(FailureClass::ProductFailure)
        ));
        for (outcome, class) in [
            ("PASS", None),
            ("HOST-INAPPLICABLE", None),
            ("FAIL", None),
            ("FAIL", Some(FailureClass::UnderstoodInfrastructureFailure)),
            ("ERROR", Some(FailureClass::NoResult)),
            ("ERROR", Some(FailureClass::UnderstoodPrerequisiteFailure)),
        ] {
            assert!(
                !cell_result_is_retryable(outcome, class),
                "{outcome} {class:?}"
            );
        }
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
    fn only_a_diagnostic_results_run_may_select_a_diagnostic_cell() {
        use serde_json::json;
        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-diagnostic-selection-{}",
            std::process::id()
        ));
        let manifests = fixture.join("tests/e2e/manifests");
        fs::create_dir_all(&manifests).unwrap();
        fs::write(
            manifests.join("defaults.yaml"),
            "schema: 3\ntimeout_seconds: 2\ncpu_timeout_seconds: 1\n",
        )
        .unwrap();
        let off = |backends: &[&str]| {
            json!({
                "ci": false,
                "ci_disabled_reason": "Only the verify cell is measured here",
                "backends_enabled": [],
                "backends_disabled": backends
                    .iter()
                    .map(|backend| (backend.to_string(), json!("Only the verify cell is measured here")))
                    .collect::<serde_json::Map<_, _>>(),
            })
        };
        let all = ["ptrace", "dbt", "kvm", "sabre", "liteinst", "in-guest-trap"];
        let test = |id: &str, diagnostic: bool| {
            let mut verify = json!({
                "ci": true,
                "backends_enabled": ["ptrace"],
                "backends_disabled": {
                    "dbt": "fixture", "kvm": "fixture", "sabre": "fixture", "liteinst": "fixture",
                    "in-guest-trap": "fixture"
                },
                "comparator": "stripped",
                "comparator_reason": "The fixture corpus uses the stripped comparison",
            });
            if diagnostic {
                verify["diagnostic"] = json!({"ptrace": "A bounded fixture probe"});
            }
            json!({
                "id": format!("pick/{id}"),
                "description": "Diagnostic selection fixture",
                "lane": "portable",
                "occasional": false,
                "direct": ["/bin/true"],
                "observation": {"status": true, "stdout": true, "stderr": true},
                "modes": {
                    "verify": verify,
                    "naked": off(&["native"]),
                    "replay": off(&all),
                    "chaos": off(&all),
                    "custom": off(&all),
                }
            })
        };
        fs::write(
            manifests.join("pick.yaml"),
            serde_json::to_vec(&json!({
                "schema": 3,
                "bucket": "pick",
                "test": [test("probe", true), test("plain", false)],
            }))
            .unwrap(),
        )
        .unwrap();
        let loaded = ManifestSet::load(&fixture).unwrap();
        let select = |id: &str| {
            loaded
                .select(&hermit_manifest_plan::runner::Selection {
                    test: Some(format!("pick/{id}")),
                    ..hermit_manifest_plan::runner::Selection::default()
                })
                .unwrap()
        };
        let probe = select("probe");
        let error = super::require_declared_diagnostics(&probe, false, true).unwrap_err();
        assert!(
            error.contains("pick/probe (verify/ptrace) is a diagnostic cell"),
            "{error}"
        );
        super::require_declared_diagnostics(&probe, true, true).unwrap();
        super::require_declared_diagnostics(&select("plain"), false, true).unwrap();
        // With no scheduler report there is no schema to misstate it, and an
        // undeclared run does not excuse its failure (excused_diagnostic).
        super::require_declared_diagnostics(&probe, false, false).unwrap();
        fs::remove_dir_all(fixture).unwrap();
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
        const CHILD_DIAGNOSTIC: &str = "HERMIT_HARNESS_RETRY_TEST_DIAGNOSTIC_RESULTS";
        const CHILD_NO_RETRY: &str = "HERMIT_HARNESS_RETRY_TEST_NO_RETRY";
        const TEST_NAME: &str = "tests::production_run_retries_only_product_failures";
        if let Some(fixture) = std::env::var_os(CHILD_FIXTURE) {
            let fixture = PathBuf::from(fixture);
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let manifests = ManifestSet::load(&fixture).unwrap();
            let mut argv: Vec<String> = vec![
                "--mode".into(),
                "naked".into(),
                "--jobs".into(),
                "2".into(),
                "--results".into(),
                fixture.join("results.jsonl").to_string_lossy().into_owned(),
                "--junit".into(),
                fixture.join("junit.xml").to_string_lossy().into_owned(),
            ];
            if std::env::var_os(CHILD_DIAGNOSTIC).is_some() {
                argv.push("--diagnostic-results".into());
            }
            if std::env::var_os(CHILD_NO_RETRY).is_some() {
                argv.push("--no-retry".into());
            }
            let args = parse(argv.into_iter());
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
                "liteinst": "This control executes native commands only",
                "in-guest-trap": "This control executes native commands only"
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
        let manifest_text =
            serde_json::to_vec(&json!({"schema": 3, "bucket": "retry", "test": recipes})).unwrap();
        fs::write(manifests.join("retry.yaml"), &manifest_text).unwrap();
        // Only the isolated child receives execution environment changes. A
        // missing Hermit path makes the optional metadata/help probes inert;
        // all five cells use the actual native execution path.
        let run_child = |fixture: &Path, diagnostic_results: bool, no_retry: bool| {
            let mut command = Command::new("timeout");
            command
                .args(["--kill-after=2s", "25s"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env(CHILD_FIXTURE, fixture)
                .env("HERMIT_BIN", fixture.join("missing-hermit"))
                .env("E2E_RESULT_ROOT", fixture.join("artifacts"))
                .env("E2E_BUILD_ROOT", fixture.join("build"))
                .env("E2E_RUN_ID", "native-retry-control")
                .env("E2E_MACHINE_SHORTNAME", "native-retry-control")
                .env("E2E_KERNEL_VERSION", "native-retry-control")
                .env("DAGRUN_TEST_COUNTS_PATH", fixture.join("counts.json"));
            if diagnostic_results {
                command.env(CHILD_DIAGNOSTIC, "1");
            }
            if no_retry {
                command.env(CHILD_NO_RETRY, "1");
            }
            command.output().unwrap()
        };
        let output = run_child(&fixture, false, false);
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
        // Without --diagnostic-results: schema 2, one terminal row per cell.
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
        // The same cells with --diagnostic-results: schema 4, every attempt with
        // its typed cause and the row's own reason.
        let diagnostic_fixture = fixture.join("diagnostic");
        fs::create_dir_all(diagnostic_fixture.join("tests/e2e/manifests")).unwrap();
        for name in ["defaults.yaml", "retry.yaml"] {
            fs::copy(
                manifests.join(name),
                diagnostic_fixture.join("tests/e2e/manifests").join(name),
            )
            .unwrap();
        }
        let diagnostic_output = run_child(&diagnostic_fixture, true, false);
        assert!(
            diagnostic_output.status.success(),
            "native diagnostic-results run control failed: {}\n{}\n{}",
            diagnostic_fixture.display(),
            String::from_utf8_lossy(&diagnostic_output.stdout),
            String::from_utf8_lossy(&diagnostic_output.stderr)
        );
        let counts: serde_json::Value =
            serde_json::from_slice(&fs::read(diagnostic_fixture.join("counts.json")).unwrap())
                .unwrap();
        let unrecorded = "no specific failure reason was recorded";
        let failed =
            |attempt: u64| json!({"attempt": attempt, "outcome": "failed", "detail": unrecorded});
        let passed =
            |attempt: u64| json!({"attempt": attempt, "outcome": "passed", "detail": null});
        let row = |id: &str, result: &str, attempt_results: Vec<serde_json::Value>| {
            json!({
                "id": id,
                "result": result,
                "attempts": attempt_results.len(),
                "attempt_results": attempt_results,
                "diagnostic_reason": null,
            })
        };
        assert_eq!(
            counts,
            json!({
                "schema": 4,
                "executed_tests": 5,
                "filtered_tests": 0,
                "results": [
                    row("retry/infra [native/naked]", "fail", vec![json!({
                        "attempt": 1,
                        "outcome": "infrastructure_error",
                        "detail": format!(
                            "cannot execute {}: No such file or directory (os error 2)",
                            missing.display()
                        ),
                    })]),
                    row("retry/pass [native/naked]", "pass", vec![passed(1)]),
                    row("retry/product [native/naked]", "fail", vec![failed(1), failed(2)]),
                    row("retry/recovers [native/naked]", "pass", vec![failed(1), passed(2)]),
                    row("retry/timeout [native/naked]", "fail", vec![json!({
                        "attempt": 1,
                        "outcome": "wall_timeout",
                        "detail": "cell exceeded 2 wall s backstop (1 s CPU budget)",
                    })]),
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
        // The same cells with --no-retry, through the real run(): every cell is
        // exactly one attempt, so the product failure is not retried and the
        // cell that recovers on its retry stays failed.
        let no_retry_fixture = fixture.join("no-retry");
        fs::create_dir_all(no_retry_fixture.join("tests/e2e/manifests")).unwrap();
        for name in ["defaults.yaml", "retry.yaml"] {
            fs::copy(
                manifests.join(name),
                no_retry_fixture.join("tests/e2e/manifests").join(name),
            )
            .unwrap();
        }
        let no_retry_output = run_child(&no_retry_fixture, false, true);
        assert!(
            no_retry_output.status.success(),
            "native --no-retry run control failed: {}\n{}\n{}",
            no_retry_fixture.display(),
            String::from_utf8_lossy(&no_retry_output.stdout),
            String::from_utf8_lossy(&no_retry_output.stderr)
        );
        let counts: serde_json::Value =
            serde_json::from_slice(&fs::read(no_retry_fixture.join("counts.json")).unwrap())
                .unwrap();
        assert_eq!(
            counts,
            json!({
                "schema": 2,
                "executed_tests": 5,
                "filtered_tests": 0,
                "results": [
                    {"id": "retry/infra [native/naked]", "result": "fail", "attempts": 1},
                    {"id": "retry/pass [native/naked]", "result": "pass", "attempts": 1},
                    {"id": "retry/product [native/naked]", "result": "fail", "attempts": 1},
                    {"id": "retry/recovers [native/naked]", "result": "fail", "attempts": 1},
                    {"id": "retry/timeout [native/naked]", "result": "fail", "attempts": 1}
                ]
            })
        );
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

    /// The production retry decision: a product FAIL of a shipped
    /// strict-compatibility row (no_retry_reason) is final, while the same
    /// FAIL of an ordinary verify cell earns its retry.
    #[test]
    fn a_no_retry_cell_is_not_retried_after_a_product_failure() {
        let manifests = ManifestSet::load(&super::root(None)).unwrap();
        let cell = |test: &str| {
            manifests
                .select(&hermit_manifest_plan::runner::Selection {
                    test: Some(test.into()),
                    mode: Some("verify".into()),
                    backend: Some("ptrace".into()),
                    population: Some(hermit_manifest_plan::runner::Population::Required),
                    ..hermit_manifest_plan::runner::Selection::default()
                })
                .unwrap()
                .remove(0)
        };
        let failed = attempt_row(
            "fixture/t",
            1,
            "FAIL",
            Some("product_failure"),
            "required",
            &[],
            None,
        );
        assert!(!super::attempt_earns_retry(
            super::Retries::Framework,
            &cell("compat/cat"),
            &failed
        ));
        assert!(super::attempt_earns_retry(
            super::Retries::Framework,
            &cell("c-programs/random-readv-stream"),
            &failed
        ));
        // --no-retry: the same retryable product failure is final.
        assert!(!super::attempt_earns_retry(
            super::Retries::Off,
            &cell("c-programs/random-readv-stream"),
            &failed
        ));
    }

    /// A verify attempt whose only failure is a divergence Hermit attributes to
    /// a host file changing during one run earns one retry, even on a
    /// strict-compatibility row (no_retry_reason), whose product failures are
    /// final. `--no-retry` still turns it off, and a plain divergence of the
    /// same row stays final, as does the same row recording a stdout
    /// declaration or relaxations other than its cell's, retaining a
    /// comparison the cell's comparator does not accept, or retaining a stderr
    /// whose first line is a failure class the executor decides before it
    /// reads any report.
    #[test]
    fn a_host_input_change_earns_one_retry_even_on_a_no_retry_cell() {
        use sha2::Digest;
        let manifests = ManifestSet::load(&super::root(None)).unwrap();
        let compat_cat = manifests
            .select(&hermit_manifest_plan::runner::Selection {
                test: Some("compat/cat".into()),
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
                population: Some(hermit_manifest_plan::runner::Population::Required),
                ..hermit_manifest_plan::runner::Selection::default()
            })
            .unwrap()
            .remove(0);
        let output = serde_json::json!({"exit_code": 0, "signal": null,
            "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stdout_bytes": 0, "stderr_bytes": 0});
        let identity = |ino: u64| {
            serde_json::json!({"dev": 33, "ino": ino, "size": 100,
            "mtime_sec": 1_700_000_000_i64, "mtime_nsec": 0})
        };
        // The comparisons Hermit records: compat/cat's verify cell selects the
        // strict comparator, so a host-input report of it records a canonical
        // comparison of non-vacuous logs; the stripped one is another
        // comparator's.
        let stripped = serde_json::json!({"strictness": "stripped", "display_name": "Stripped",
            "compare_logs": true, "compare_io_buffers": true, "log_scope": "deterministic",
            "record_envelope": "all_records_v1", "virtualize_time": true,
            "strip_lines": true, "canonicalize_addresses": false, "full_trace": false,
            "exact_remainder": false,
            "stripped_prefixes": ["real-wall-clock-prefix/v1",
                "unsafe-numeric-address-and-path-normalization/v1"],
            "canonicalizations": [], "ignore_lines": false, "skip_commit": false,
            "skip_detlog": false});
        let canonical = serde_json::json!({"strictness": "canonical",
            "display_name": "BitwiseInfoV1",
            "compare_logs": true, "compare_io_buffers": true, "log_scope": "info",
            "record_envelope": "all_records_v1", "virtualize_time": true,
            "strip_lines": false, "canonicalize_addresses": true, "full_trace": true,
            "exact_remainder": true, "stripped_prefixes": ["real-wall-clock-prefix/v1"],
            "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
            "ignore_lines": false, "skip_commit": false, "skip_detlog": false});
        let report_of = |comparison: &serde_json::Value, messages: u64| {
            serde_json::to_string(&serde_json::json!({
                "verified": false, "bitwise_parity": false, "verdict": "infrastructure_error",
                "no_result_reason": null,
                "infrastructure_error": {"kind": "host_input_changed", "run": "run1",
                    "path": "/etc/ld.so.cache", "before": identity(57_899_095),
                    "after": identity(57_907_831)},
                "comparison": comparison,
                "compared_log_messages": {"left": messages, "right": messages},
                "compared_outputs": {"left": output, "right": output},
                "guest_exit_code": 0, "guest_signal": null,
                "first_divergent_scheduler_turn": 4, "first_divergent_virtual_nanoseconds": 7,
                "first_divergent_record": 9, "first_divergent_syscall": 2,
                "first_divergent_left_message": "left", "first_divergent_right_message": "right"
            }))
            .unwrap()
        };
        let bytes = report_of(&canonical, 2);
        let parsed =
            hermit_manifest_plan::canonical_verdict::VerificationReport::from_current_json_slice(
                bytes.as_bytes(),
            )
            .unwrap();
        let reason = hermit_manifest_plan::runner::host_input_changed_reason(
            parsed.infrastructure_error.as_ref().unwrap(),
        );
        let row = |reason: &str, report: &str| -> CellResult {
            let digest = format!("{:x}", sha2::Sha256::digest(report.as_bytes()));
            let attempt = serde_json::json!({"index": "1", "outcome": "ERROR",
                "error_kind": "infrastructure", "status": 1, "signal": null,
                "timed_out": false, "duration_ms": 1, "argv": [], "guest_argv": [],
                "env": {}, "cwd": "/repo", "shell_command": "", "stdout": "", "stderr": "",
                "reason": reason, "verification_report": report,
                "verification_report_sha256": digest});
            let mut row = serde_json::json!({
                "schema": 4, "run_id": "fixture", "hermit_sha": "sha", "source_tree_dirty": false,
                "test": "compat/cat", "category": "compat", "lane": "portable", "mode": "verify",
                "backend": "ptrace", "classification": "required", "outcome": "ERROR",
                "failure_class": "understood_infrastructure_failure",
                "error_kind": "infrastructure", "attempt": 1,
                "reason": reason, "argv": [], "guest_argv": [], "env": {}, "cwd": "/repo",
                "shell_command": "", "artifact_dir": "/repo/a",
                "declared_stdout": {"exact": null, "contains": null}
            });
            // The cell's relaxations, as an executed row of it records them.
            row["relaxations"] = serde_json::json!(super::cell_relaxations(&compat_cat));
            row["attempts"] = serde_json::json!([attempt]);
            serde_json::from_value(row).unwrap()
        };
        let changed = row(&reason, &bytes);
        assert_eq!(
            super::attempt_retry_cause(super::Retries::Framework, &compat_cat, &changed),
            Some(super::RetryCause::HostInputChanged)
        );
        assert_eq!(
            super::attempt_retry_cause(super::Retries::Off, &compat_cat, &changed),
            None
        );
        // The same row with a reason its report does not produce, or labelled
        // a product failure, is not a host input change and stays final on
        // this cell.
        let relabelled = row("host input changed", &bytes);
        assert_eq!(
            super::attempt_retry_cause(super::Retries::Framework, &compat_cat, &relabelled),
            None
        );
        let mut product = changed.clone();
        product.outcome = "FAIL".into();
        product.failure_class = Some(hermit_manifest_plan::runner::FailureClass::ProductFailure);
        assert_eq!(
            super::attempt_retry_cause(super::Retries::Framework, &compat_cat, &product),
            None
        );
        // The rule checks the stdout assertions the row records, so a row
        // recording another stdout declaration or other relaxations than its
        // cell's (an imported row written under another manifest) earns no
        // retry, though the rule alone would admit it.
        let mut redeclared = changed.clone();
        redeclared.declared_stdout = Some(hermit_manifest_plan::runner::DeclaredStdout {
            exact: None,
            contains: Some(String::new()),
        });
        let mut relaxed = changed.clone();
        relaxed
            .relaxations
            .push("--no-rcb-time: fixture reason".into());
        for row in [&redeclared, &relaxed] {
            assert!(super::host_input_change_only(row), "{row:#?}");
            assert_eq!(
                super::attempt_retry_cause(super::Retries::Framework, &compat_cat, row),
                None,
                "{row:#?}"
            );
        }
        // A report whose comparison the cell's comparator does not accept (one
        // that compared no log messages, or another comparator's comparison),
        // correctly hashed and with the plain host-input reason, is not a host
        // input change either: the classifier calls such an attempt
        // incomplete verification evidence, so a row it did not classify must
        // not claim the retry by its labels.
        for report in [report_of(&canonical, 0), report_of(&stripped, 2)] {
            let incomplete = row(&reason, &report);
            assert!(
                !super::host_input_change_only(&incomplete),
                "{incomplete:#?}"
            );
            assert_eq!(
                super::attempt_retry_cause(super::Retries::Framework, &compat_cat, &incomplete),
                None,
                "{incomplete:#?}"
            );
        }
        // The host-input line Hermit itself writes to stderr does not
        // disqualify the attempt.
        let mut announced = changed.clone();
        announced.attempts[0].stderr =
            "HERMIT_HOST_INPUT_CHANGED /etc/ld.so.cache changed during run1\n".into();
        assert_eq!(
            super::attempt_retry_cause(super::Retries::Framework, &compat_cat, &announced),
            Some(super::RetryCause::HostInputChanged)
        );
        // A retained stderr whose first line is a failure class the executor
        // decides before it reads any report (a refused launch, the cell's
        // backend unavailable, an unavailable-backend line naming another
        // backend, an unclassified internal failure) is not a host input
        // change, though the report and reason are unchanged: the executor
        // classifies such an attempt by that line, so a row it did not
        // classify, such as an imported one, must not claim the retry by its
        // report.
        for line in [
            "HERMIT_INTERNAL_FAILURE class=guest-program-not-found",
            "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=ptrace",
            "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=kvm",
            "HERMIT_INTERNAL_FAILURE class=cli-error",
        ] {
            let mut refused = changed.clone();
            refused.attempts[0].stderr = format!("{line}\n");
            assert!(!super::host_input_change_only(&refused), "{line}");
            assert_eq!(
                super::attempt_retry_cause(super::Retries::Framework, &compat_cat, &refused),
                None,
                "{line}"
            );
        }
    }

    /// The production retry decision for a replay cell: its product FAIL (a
    /// replay that diverged from its recording) is final, as the retired rr
    /// lane ran each program once, while the same FAIL of the same test's
    /// verify cell earns its retry.
    #[test]
    fn a_replay_cell_is_not_retried_after_a_product_failure() {
        let manifests = ManifestSet::load(&super::root(None)).unwrap();
        let cell = |mode: &str| {
            manifests
                .select(&hermit_manifest_plan::runner::Selection {
                    test: Some("c-programs/random-readv-stream".into()),
                    mode: Some(mode.into()),
                    backend: Some("ptrace".into()),
                    population: Some(hermit_manifest_plan::runner::Population::Required),
                    ..hermit_manifest_plan::runner::Selection::default()
                })
                .unwrap()
                .remove(0)
        };
        let failed = attempt_row(
            "fixture/t",
            1,
            "FAIL",
            Some("product_failure"),
            "required",
            &[],
            None,
        );
        let replay = cell("replay");
        assert_eq!(replay.id.mode, "replay");
        assert_eq!(
            super::attempt_retry_cause(super::Retries::Framework, &replay, &failed),
            None
        );
        assert!(super::attempt_earns_retry(
            super::Retries::Framework,
            &cell("verify"),
            &failed
        ));
    }

    #[test]
    fn no_retry_flag_turns_framework_retries_off() {
        assert_eq!(
            super::parse(std::iter::empty()).retries,
            super::Retries::Framework
        );
        assert_eq!(
            super::parse(["--no-retry".to_string()].into_iter()).retries,
            super::Retries::Off
        );
    }

    #[test]
    fn source_snapshot_flags_parse_once() {
        let args = super::parse(
            [
                "--repo-root",
                "/snapshot",
                "--source-sha",
                "03bbb83581fad247251df6363f50e61e24c2957e",
                "--tpx-json",
                "/out/tpx.jsonl",
            ]
            .map(String::from)
            .into_iter(),
        );
        assert_eq!(
            args.repo_root.as_deref(),
            Some(std::path::Path::new("/snapshot"))
        );
        assert_eq!(
            args.source_sha.as_deref(),
            Some("03bbb83581fad247251df6363f50e61e24c2957e")
        );
        assert_eq!(
            args.tpx_json.as_deref(),
            Some(std::path::Path::new("/out/tpx.jsonl"))
        );
        let defaults = super::parse(std::iter::empty());
        assert!(defaults.repo_root.is_none() && defaults.source_sha.is_none());
        assert!(defaults.tpx_json.is_none());
    }

    /// One HPHP record per final cell, named `test/mode@backend`, whose status
    /// follows the run's verdict: an excused diagnostic or host-inapplicable
    /// cell is skipped (never passed), and the record still says why.
    #[test]
    fn tpx_json_reports_each_final_cell_with_the_run_verdict() {
        let rows = [
            attempt_row("t/pass", 1, "PASS", None, "required", &[], None),
            attempt_row(
                "t/fail",
                1,
                "FAIL",
                Some("product_failure"),
                "required",
                &[],
                Some("diverged"),
            ),
            attempt_row(
                "t/diag",
                1,
                "FAIL",
                Some("product_failure"),
                "diagnostic",
                &[],
                Some("bounded"),
            ),
            attempt_row(
                "t/hi",
                1,
                "HOST-INAPPLICABLE",
                Some("understood_prerequisite_failure"),
                "required",
                &[],
                Some("no cpuid"),
            ),
            attempt_row(
                "t/err",
                1,
                "ERROR",
                Some("no_result"),
                "required",
                &[],
                None,
            ),
        ];
        let path = std::env::temp_dir().join(format!("tpx-json-{}.jsonl", std::process::id()));
        super::write_tpx_json(&path, &rows, &[false, false, true, false, false], &[], &[]).unwrap();
        let records = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        fs::remove_file(&path).unwrap();
        let reported = records
            .iter()
            .filter(|record| record["op"] == "test_done")
            .map(|record| {
                let details: serde_json::Value =
                    serde_json::from_str(record["details"].as_str().unwrap()).unwrap();
                (
                    record["test"].as_str().unwrap().to_string(),
                    record["status"].as_str().unwrap().to_string(),
                    details["outcome"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            ("t/pass/verify@ptrace", "passed", "PASS"),
            ("t/fail/verify@ptrace", "failed", "FAIL"),
            ("t/diag/verify@ptrace", "skipped", "FAIL"),
            ("t/hi/verify@ptrace", "skipped", "HOST-INAPPLICABLE"),
            ("t/err/verify@ptrace", "failed", "ERROR"),
        ]
        .map(|(test, status, outcome)| (test.to_string(), status.to_string(), outcome.to_string()));
        assert_eq!(reported, expected);
        assert_eq!(
            records.last().unwrap(),
            &serde_json::json!({"op": "all_done"})
        );
        // Only the keys TestX accepts: an unknown top-level key fails the whole Tpx run.
        for record in &records[..records.len() - 1] {
            let mut keys = record
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            assert_eq!(keys, ["details", "op", "status", "test"]);
        }
    }

    /// Read a Tpx file back as `(test, status, details)` per `test_done`
    /// record, checking the record keys and the closing `all_done`.
    fn read_tpx(path: &std::path::Path) -> Vec<(String, String, serde_json::Value)> {
        let records = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            records.last().unwrap(),
            &serde_json::json!({"op": "all_done"})
        );
        records[..records.len() - 1]
            .iter()
            .map(|record| {
                let mut keys = record
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                keys.sort();
                assert_eq!(keys, ["details", "op", "status", "test"]);
                assert_eq!(record["op"], "test_done");
                (
                    record["test"].as_str().unwrap().to_string(),
                    record["status"].as_str().unwrap().to_string(),
                    serde_json::from_str(record["details"].as_str().unwrap()).unwrap(),
                )
            })
            .collect()
    }

    /// A run can fail without a failed cell. Each selected cell that returned
    /// no result is a failed record named by its identity, and the run-level
    /// reasons are one failed run-verdict record, so Tpx sees the failure.
    #[test]
    fn tpx_json_reports_missing_cells_and_run_failures_as_failed() {
        let rows = [attempt_row(
            "t/pass",
            1,
            "PASS",
            None,
            "required",
            &[],
            None,
        )];
        let missing = ["t/gone/verify@kvm".to_string()];
        let failures = [
            "only 1 of 2 selected cells returned a result".to_string(),
            "cannot write DAGRUN_TEST_COUNTS_PATH: disk full".to_string(),
        ];
        let path = std::env::temp_dir().join(format!("tpx-missing-{}.jsonl", std::process::id()));
        super::write_tpx_json(&path, &rows, &[false], &missing, &failures).unwrap();
        let records = read_tpx(&path);
        fs::remove_file(&path).unwrap();
        let reported = records
            .iter()
            .map(|(test, status, _)| (test.as_str(), status.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            reported,
            [
                ("t/pass/verify@ptrace", "passed"),
                ("t/gone/verify@kvm", "failed"),
                (super::TPX_RUN_VERDICT_TEST, "failed"),
            ]
        );
        assert_eq!(
            records[1].2["reason"],
            "the selected cell returned no result"
        );
        assert_eq!(records[2].2["run_failures"], serde_json::json!(failures));
        // A run with neither adds no record beyond its cells.
        super::write_tpx_json(&path, &rows, &[false], &[], &[]).unwrap();
        let records = read_tpx(&path);
        fs::remove_file(&path).unwrap();
        assert_eq!(records.len(), 1);
    }

    /// A run whose every cell was host-inapplicable executed nothing and is
    /// refused; the refusal is a run failure, which fails the exit status and
    /// is a failed Tpx record. A single inapplicable cell must not read as a
    /// skipped (green) Tpx run while the harness exits 1.
    #[test]
    fn a_lone_host_inapplicable_cell_fails_the_run_in_tpx_too() {
        assert_eq!(super::vacuity_refusal(0, 0), None);
        assert_eq!(super::vacuity_refusal(2, 1), None);
        let refusal = super::vacuity_refusal(1, 1).unwrap();
        assert!(refusal.contains("not a pass"), "{refusal}");
        let rows = [attempt_row(
            "t/hi",
            1,
            "HOST-INAPPLICABLE",
            Some("understood_prerequisite_failure"),
            "required",
            &[],
            Some("no kvm"),
        )];
        let path = std::env::temp_dir().join(format!("tpx-vacuous-{}.jsonl", std::process::id()));
        super::write_tpx_json(&path, &rows, &[false], &[], std::slice::from_ref(&refusal)).unwrap();
        let records = read_tpx(&path);
        fs::remove_file(&path).unwrap();
        let reported = records
            .iter()
            .map(|(test, status, _)| (test.as_str(), status.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            reported,
            [
                ("t/hi/verify@ptrace", "skipped"),
                (super::TPX_RUN_VERDICT_TEST, "failed"),
            ]
        );
        assert_eq!(records[1].2["run_failures"], serde_json::json!([refusal]));
    }

    /// The real run(): one passing native cell, but the scheduler test-count
    /// file cannot be written. The run exits 1 and its Tpx output says so with
    /// a failed run-verdict record, not just a passed cell.
    #[test]
    fn an_unwritable_count_file_fails_the_run_in_tpx_too() {
        use std::path::Path;
        use std::path::PathBuf;
        use std::process::Command;
        use std::process::ExitCode;

        use serde_json::json;

        const CHILD_FIXTURE: &str = "HERMIT_HARNESS_TPX_COUNTS_TEST_FIXTURE";
        const TEST_NAME: &str = "tests::an_unwritable_count_file_fails_the_run_in_tpx_too";
        if let Some(fixture) = std::env::var_os(CHILD_FIXTURE) {
            let fixture = PathBuf::from(fixture);
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let manifests = ManifestSet::load(&fixture).unwrap();
            let argv: Vec<String> = vec![
                "--mode".into(),
                "naked".into(),
                "--results".into(),
                fixture.join("results.jsonl").to_string_lossy().into_owned(),
                "--junit".into(),
                fixture.join("junit.xml").to_string_lossy().into_owned(),
                "--tpx-json".into(),
                fixture.join("tpx.jsonl").to_string_lossy().into_owned(),
            ];
            let args = parse(argv.into_iter());
            super::validate_args("run", &args);
            assert_eq!(super::run(&root, &manifests, &args), ExitCode::FAILURE);
            return;
        }

        let fixture =
            std::env::temp_dir().join(format!("hermit-harness-tpx-counts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fixture);
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
                "liteinst": "This control executes native commands only",
                "in-guest-trap": "This control executes native commands only"
            }
        });
        let manifest = json!({
            "schema": 3,
            "bucket": "counts",
            "test": [{
                "id": "counts/pass",
                "description": "Native Tpx run-verdict control",
                "lane": "portable",
                "occasional": false,
                "direct": ["/bin/true"],
                "observation": {"status": true, "stdout": true, "stderr": true},
                "modes": {
                    "naked": {
                        "ci": false,
                        "ci_disabled_reason": "Native Tpx control is explicitly selected",
                        "backends_enabled": ["native"],
                        "runs": 1,
                        "assert": {"min_distinct": 1}
                    },
                    "verify": disabled,
                    "chaos": disabled,
                    "replay": disabled,
                    "custom": disabled
                }
            }]
        });
        fs::write(
            manifests.join("counts.yaml"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
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
            .env("E2E_RUN_ID", "tpx-counts-control")
            .env("E2E_MACHINE_SHORTNAME", "tpx-counts-control")
            .env("E2E_KERNEL_VERSION", "tpx-counts-control")
            // Its directory does not exist, so publishing the counts fails.
            .env(
                "DAGRUN_TEST_COUNTS_PATH",
                fixture.join("no-such-dir/counts.json"),
            )
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the child must exit 1 from run(): {}\n{}\n{stderr}",
            fixture.display(),
            String::from_utf8_lossy(&output.stdout),
        );
        let records = read_tpx(&fixture.join("tpx.jsonl"));
        let reported = records
            .iter()
            .map(|(test, status, _)| (test.as_str(), status.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            reported,
            [
                ("counts/pass/naked@native", "passed"),
                (super::TPX_RUN_VERDICT_TEST, "failed"),
            ],
            "{stderr}"
        );
        let failures = records[1].2["run_failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0]
                .as_str()
                .unwrap()
                .starts_with("cannot write DAGRUN_TEST_COUNTS_PATH: "),
            "{failures:?}"
        );
        fs::remove_dir_all(fixture).unwrap();
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

        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("the committed validation DAG must parse");
        for lane in ["portable", "privileged"] {
            let dag = dagrun::select_steps_by_labels(&committed, &[lane.to_string()]).unwrap();
            let inner =
                format!("target/debug/test-harness build --lane {lane} --ci-only --allow-empty");
            let matches = dag
                .steps
                .iter()
                .filter(|step| command_runs_exactly(&step.cmd, &inner))
                .collect::<Vec<_>>();
            assert_eq!(matches.len(), 2, "{lane}: host and pinned build commands");
            let pinned = matches
                .iter()
                .find(|step| step.cmd.starts_with(PINNED_COMMAND_PREFIX))
                .expect("the generated pinned command must match");
            for malformed in [
                pinned
                    .cmd
                    .replace("/src/ci/hermetic/assert-no-network.sh && ", ""),
                pinned
                    .cmd
                    .replace("/src/ci/hermetic/assert-build-dependencies.sh && ", ""),
                pinned
                    .cmd
                    .replace("hermit_payload=$1", "hermit_payload=true"),
                format!("{} --unexpected", pinned.cmd),
                pinned.cmd.replace(
                    "--env E2E_RESULT_ROOT ",
                    "--env E2E_RESULT_ROOT --env E2E_RESULT_ROOT ",
                ),
                pinned
                    .cmd
                    .replace("--env E2E_RESULT_ROOT ", "--env lowercase "),
                pinned.cmd.replace(&inner, &format!("{inner} --jobs 2")),
            ] {
                assert_ne!(malformed, pinned.cmd, "control must alter the command");
                assert!(
                    !command_runs_exactly(&malformed, &inner),
                    "accepted malformed or nonmatching command: {malformed}"
                );
            }
        }
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

    /// A fresh temporary root holding a copy of tests/e2e, the expected plan
    /// and the optional-cell inventory, the files `sync-cells` reads and writes, with every other entry of
    /// the checkout, tests and ci linked in for the programs the manifests name.
    fn sync_cells_fixture(label: &str) -> std::path::PathBuf {
        fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
            fs::create_dir_all(to).unwrap();
            for entry in fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let target = to.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    fs::copy(entry.path(), &target).unwrap();
                }
            }
        }
        let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let fixture = std::env::temp_dir().join(format!(
            "hermit-harness-sync-cells-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&fixture);
        let link_except = |dir: &str, own: &[&str]| {
            fs::create_dir_all(fixture.join(dir)).unwrap();
            for entry in fs::read_dir(checkout.join(dir)).unwrap() {
                let name = entry.unwrap().file_name();
                if !own.iter().any(|own| name == *own) {
                    std::os::unix::fs::symlink(
                        checkout.join(dir).join(&name),
                        fixture.join(dir).join(&name),
                    )
                    .unwrap();
                }
            }
        };
        // Not .git: the fixture is no checkout.
        link_except("", &["tests", ".git"]);
        fs::remove_file(fixture.join("ci")).unwrap();
        link_except("ci", &["expected-e2e-plan.json", "optional-e2e-cells.txt"]);
        link_except("tests", &["e2e"]);
        copy_tree(&checkout.join("tests/e2e"), &fixture.join("tests/e2e"));
        for relative in [super::EXPECTED_PLAN_PATH, super::OPTIONAL_CELLS_PATH] {
            // A missing inventory is copied as missing, and `sync` reports it stale.
            if checkout.join(relative).exists() {
                fs::copy(checkout.join(relative), fixture.join(relative)).unwrap();
            }
        }
        fixture
    }

    /// `source`, a manifest file, with `backend` taken out of `test`'s `mode`:
    /// the reverse of a cell flip. Returns the edited YAML.
    fn unflip(source: &str, test: &str, mode: &str, backend: &str) -> String {
        use serde_yaml::Value;
        let mut manifest: Value = serde_yaml::from_str(source).unwrap();
        if manifest.get("corpus").is_some() {
            return unflip_corpus(manifest, test, mode, backend);
        }
        let entry = manifest["test"]
            .as_sequence_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["id"].as_str() == Some(test))
            .unwrap_or_else(|| panic!("{test} is not in its manifest"));
        let mode = entry["modes"][mode].as_mapping_mut().unwrap();
        let enabled = mode
            .get_mut("backends_enabled")
            .and_then(Value::as_sequence_mut)
            .unwrap();
        let before = enabled.len();
        enabled.retain(|name| name.as_str() != Some(backend));
        assert_eq!(
            enabled.len() + 1,
            before,
            "{test} does not enable {backend}"
        );
        // A per-backend hermit_args_reason is keyed like hermit_args.
        for per_backend in [
            "ci",
            "expected_stdout",
            "hermit_args",
            "hermit_args_reason",
            "env_by_backend",
        ] {
            if let Some(map) = mode.get_mut(per_backend).and_then(Value::as_mapping_mut) {
                map.remove(backend);
            }
        }
        // Hermit flags may name only enabled backends, and their reason may
        // not outlive the last of them.
        if mode
            .get("hermit_args")
            .and_then(Value::as_mapping)
            .is_some_and(serde_yaml::Mapping::is_empty)
        {
            mode.remove("hermit_args");
            mode.remove("hermit_args_reason");
        }
        let disabled = mode
            .entry("backends_disabled".into())
            .or_insert_with(|| Value::Mapping(Default::default()));
        disabled.as_mapping_mut().unwrap().insert(
            backend.into(),
            "un-flipped by the sync-cells round-trip test".into(),
        );
        serde_yaml::to_string(&manifest).unwrap()
    }

    /// A corpus manifest's cell flip on an additional backend is the row's
    /// label in that backend's `additional` entry, so the reverse takes the
    /// label out of it, and the entry out of the list once it names no row.
    fn unflip_corpus(
        mut manifest: serde_yaml::Value,
        test: &str,
        mode: &str,
        backend: &str,
    ) -> String {
        use serde_yaml::Value;
        assert_eq!(mode, "verify", "{test}: a corpus flip is a verify cell");
        let suffix = test.split_once('/').map_or(test, |(_, suffix)| suffix);
        let label = manifest["corpus"]["rows"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|row| {
                row.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| row["label"].as_str().unwrap())
                    == suffix
            })
            .unwrap_or_else(|| panic!("{test} is no corpus row"))["label"]
            .as_str()
            .unwrap()
            .to_owned();
        // A row's cell on the corpus's own backend is full's while the row
        // names no run type; naming a focused one takes it out of full, as
        // https://github.com/rrnewton/hermit/issues/4269 put it in by removing
        // that label.
        if manifest["corpus"]["backend"].as_str() == Some(backend) {
            let row = manifest["corpus"]["rows"]
                .as_sequence_mut()
                .unwrap()
                .iter_mut()
                .find(|row| row["label"].as_str() == Some(label.as_str()))
                .unwrap();
            assert!(row.get("labels").is_none(), "{test} is not full's");
            row.as_mapping_mut().unwrap().insert(
                Value::from("labels"),
                Value::Sequence(vec![Value::from("sabre-compat-only")]),
            );
            return serde_yaml::to_string(&manifest).unwrap();
        }
        let entries = manifest["corpus"]["additional"]
            .as_sequence_mut()
            .unwrap_or_else(|| panic!("{test}: the corpus has no additional backend"));
        let entry = entries
            .iter_mut()
            .find(|entry| entry["backend"].as_str() == Some(backend))
            .unwrap_or_else(|| panic!("{test}: {backend} is no additional backend"));
        let rows = entry["rows"].as_sequence_mut().unwrap();
        let before = rows.len();
        rows.retain(|row| row.as_str() != Some(label.as_str()));
        assert_eq!(rows.len() + 1, before, "{test} does not enable {backend}");
        let no_rows_left = rows.is_empty();
        if let Some(env) = entry.get_mut("row_env").and_then(Value::as_mapping_mut) {
            env.remove(label.as_str());
        }
        if no_rows_left {
            entries.retain(|entry| entry["backend"].as_str() != Some(backend));
        }
        serde_yaml::to_string(&manifest).unwrap()
    }

    fn sync(root: &std::path::Path) -> Vec<&'static str> {
        let files = super::synced_cell_files(root).unwrap();
        let stale = super::stale_files(root, &files);
        for (relative, text) in &files {
            fs::write(root.join(relative), text).unwrap();
        }
        stale
    }

    /// Un-flipping a required cell and flipping it back with `sync-cells`
    /// gives back the committed files byte for byte
    /// (https://github.com/rrnewton/hermit/issues/3606).
    ///
    /// The subject is the plan's last row because the generator keeps the
    /// committed order and appends new rows, so a re-flipped row returns to the
    /// end of the plan: the order a hand flip by the generator gives too. Only
    /// the plan changes: the parity population has no file of its own.
    #[test]
    fn sync_cells_round_trips_an_unflip_and_reflip_byte_for_byte() {
        let root = sync_cells_fixture("tail");
        let committed_plan = fs::read(root.join(super::EXPECTED_PLAN_PATH)).unwrap();
        assert!(
            sync(&root).is_empty(),
            "the committed plan is stale; run ci/sync-cell-config.sh"
        );

        let plan: serde_json::Value = serde_json::from_slice(&committed_plan).unwrap();
        let tail = plan["cells"].as_array().unwrap().last().unwrap().clone();
        let field = |name: &str| tail[name].as_str().unwrap().to_owned();
        let (test, mode, backend) = (field("test"), field("mode"), field("backend"));
        let manifest = root.join(format!("tests/e2e/manifests/{}.yaml", field("category")));
        let original = fs::read_to_string(&manifest).unwrap();

        fs::write(&manifest, unflip(&original, &test, &mode, &backend)).unwrap();
        assert_eq!(sync(&root), [super::EXPECTED_PLAN_PATH]);
        let unflipped: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(super::EXPECTED_PLAN_PATH)).unwrap())
                .unwrap();
        let cells = unflipped["cells"].as_array().unwrap();
        assert_eq!(cells.len() + 1, plan["cells"].as_array().unwrap().len());
        assert!(
            !cells.contains(&tail),
            "the un-flipped row stayed in the plan"
        );

        fs::write(&manifest, &original).unwrap();
        assert_eq!(sync(&root), [super::EXPECTED_PLAN_PATH]);
        assert_eq!(
            fs::read(root.join(super::EXPECTED_PLAN_PATH)).unwrap(),
            committed_plan
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Disabling an optional (`ci: false`) cell changes no required plan row,
    /// parity cell or scorecard row, so only the optional-cell inventory
    /// records it; re-enabling it gives the inventory back byte for byte.
    #[test]
    fn sync_cells_records_disabling_an_optional_cell() {
        let root = sync_cells_fixture("optional");
        let committed = fs::read_to_string(root.join(super::OPTIONAL_CELLS_PATH)).unwrap();
        let line = "c-programs/writev-determinism custom ptrace";
        assert!(
            committed.lines().any(|entry| entry == line),
            "{line} is not optional"
        );
        let manifest = root.join("tests/e2e/manifests/c-programs.yaml");
        let original = fs::read_to_string(&manifest).unwrap();

        fs::write(
            &manifest,
            unflip(
                &original,
                "c-programs/writev-determinism",
                "custom",
                "ptrace",
            ),
        )
        .unwrap();
        assert_eq!(sync(&root), [super::OPTIONAL_CELLS_PATH]);
        let disabled = fs::read_to_string(root.join(super::OPTIONAL_CELLS_PATH)).unwrap();
        assert_eq!(disabled.lines().count() + 1, committed.lines().count());
        assert!(!disabled.lines().any(|entry| entry == line));

        fs::write(&manifest, &original).unwrap();
        assert_eq!(sync(&root), [super::OPTIONAL_CELLS_PATH]);
        assert_eq!(
            fs::read_to_string(root.join(super::OPTIONAL_CELLS_PATH)).unwrap(),
            committed
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
