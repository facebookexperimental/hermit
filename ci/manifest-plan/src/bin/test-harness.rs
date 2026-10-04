/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

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
use hermit_manifest_plan::parity;
use hermit_manifest_plan::parity::ParityCellId;
use hermit_manifest_plan::runner::CellId;
use hermit_manifest_plan::runner::CellResult;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::MAX_ATTEMPTS_PER_CELL;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::SelectedCell;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::append_result;
use hermit_manifest_plan::runner::backend_capability;
use hermit_manifest_plan::runner::cell_result_after_retries;
use hermit_manifest_plan::runner::cell_result_and_attempts_after_retries;
use hermit_manifest_plan::runner::checked_add_cpu_usage;
use hermit_manifest_plan::runner::diagnostic_failure_reason;
use hermit_manifest_plan::runner::host_inapplicable_result;
use hermit_manifest_plan::runner::is_diagnostic_cell;
use hermit_manifest_plan::runner::prepare_result_path;
use hermit_manifest_plan::runner::requires_capability;
use hermit_manifest_plan::runner::retries_product_failures;
use hermit_manifest_plan::runner::run_cell;
use hermit_manifest_plan::runner::validate_source_sha;
use hermit_manifest_plan::runner::write_junit;
use hermit_manifest_plan::self_test_selection;
use hermit_manifest_plan::stress_series::HostCapabilities;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapability;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapabilityVerdict;
use hermit_manifest_plan::validation_dag::PINNED_ROOT_COMMAND_GUARD;
use hermit_manifest_plan::validation_dag::TOOL_SELF_TESTS;
use hermit_manifest_plan::validation_dag::ToolSelfTest;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

const EXPECTED_PLAN_SCHEMA: u64 = 1;
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
  parity compare                   Measure parity cells from a finished run's retained logs
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
  --source-sha <SHA>               DIR is a clean `git archive` of commit SHA with no Git
                                   metadata: take the test inventory from its files and
                                   record SHA as hermit_sha instead of asking git

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
  E2E_KEEP_VERIFY_LOGS=1                 Retain successful verification logs
  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=<N> Positive finite CPU-time multiplier
  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=<N> Positive finite wall-time multiplier";

/// Read by `run` only: the parity post-pass after the determinism cells.
const RUN_ENVIRONMENT: &str =
    "  E2E_PARITY_SELECT=<TEST@BACKEND,...>   Also measure these parity cells after the run
  E2E_PARITY_POST_PASS=0                 Skip the parity post-pass (default: 1)";

const PARITY_HELP: &str = "\
Usage: test-harness parity compare --artifacts <DIR> --cell <TEST@BACKEND> [OPTIONS]

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
retained log is gone.

Options:
  --artifacts <DIR>                The directory holding the run's results.jsonl
  --cell <TEST@BACKEND>            A parity cell to measure; repeatable
  --output <PATH>                  Write records to PATH (default: DIR/parity-compare/parity.jsonl)
  --jobs <N>                       Run at most N comparisons concurrently (default: 1)
  -h, --help                       Print this help

Environment:
  HERMIT_BIN=<PATH>                      Hermit executable; a relative path is under the
                                         repository root (default: target/debug/hermit)";

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
    "  --source-sha <SHA>               DIR is a clean `git archive` of commit SHA with no Git
                                   metadata: take the test inventory from its files and
                                   record SHA as hermit_sha instead of asking git";

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

/// Why a cell cannot run on this machine, if it cannot: a capability its
/// `requires` tokens or its backend need is proven absent.
fn host_inapplicable_reason(
    requires: &[String],
    backend: Option<&str>,
    verdicts: &HostCapabilities,
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
    if args
        .selection
        .backend
        .as_deref()
        .is_some_and(|backend| !matches!(backend, "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst"))
    {
        fail("--backend must name a Hermit backend");
    }
    if args.selection.exclude_backends.iter().any(|backend| {
        !matches!(
            backend.as_str(),
            "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst"
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
        if args.repo_root.is_none() {
            fail(
                "--source-sha describes a --repo-root source snapshot; pass --repo-root DIR \
                 naming the `git archive` of that commit",
            );
        }
        validate_source_sha(sha).unwrap_or_else(|error| fail(error));
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
    if command == "parity" {
        let request = parse_parity_compare(values);
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
        fail("required E2E plan changed; update ci/expected-e2e-plan.json in the same review");
    }
    cell_count
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

// 3900 = 3780 plus the 120 seconds setup.manifest_plan's wall cap grew (180 to
// 300) in https://github.com/rrnewton/hermit/issues/3381.
const PORTABLE_PREFLIGHT_CRITICAL_PATH_SECONDS: u64 = 3900;
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
    const VALIDATION_PRIVILEGED: &str = "env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v";
    const STANDALONE_PRIVILEGED: &str = "if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n  echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n  exit 2\nfi\ntimeout --foreground --kill-after=10s 2160s env -u DAGRUN_BIN DAGRUN_ENGINE=rust ci/run-dag.sh privileged -j 2 --unsafe-no-cgroups --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v";
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

/// Whether this finished attempt of `cell` is retried: a retryable product
/// failure of a cell that has not declared `no_retry_reason`, in a run that
/// has not turned retries off.
fn attempt_earns_retry(retries: Retries, cell: &SelectedCell, result: &CellResult) -> bool {
    retries == Retries::Framework
        && retries_product_failures(cell)
        && cell_result_is_retryable(result.outcome.as_str(), result.failure_class)
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
    let cells = run_cells(manifests, args).unwrap_or_else(|e| fail(e));
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
    let planned_verify = planned_verify(&cells);
    let parity_scope = parity_scope(root, manifests, &planned_verify);
    let context = RunContext::from_env(
        root.to_path_buf(),
        args.prebuilt,
        args.source_sha.as_deref(),
    )
    .unwrap_or_else(|e| fail(e))
    .with_scheduled_worker_capacity(capacity)
    .with_parity_retained(parity::retention_closure(&parity_scope, &planned_verify));
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
            if let Some((_, reason)) = host_inapplicable_reason(
                &cell.test.requires,
                cell.id.backend.as_deref(),
                &context.host_capabilities,
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
    let summary = serde_json::json!({
        "schema": 1,
        "cells": results.len(),
        "passed": results.iter().filter(|result| result.outcome == "PASS").count(),
        "failed": results.iter().filter(|result| result.outcome == "FAIL").count(),
        "errors": results.iter().filter(|result| result.outcome == "ERROR").count(),
        // A subset of `failed`: product failures of diagnostic cells, which do
        // not fail the run. Counted and named separately so they stay visible.
        "diagnostic_failures": diagnostic_cells.len(),
        "diagnostic_failure_cells": diagnostic_cells,
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

/// The parity cells this run reports on: the committed selection and
/// `E2E_PARITY_SELECT`, resolved by [`parity::resolve_scope`], the same rule
/// the pressure test uses: the cells with a verify side planned here. An
/// explicit cell with neither side planned is dropped with a warning.
/// `E2E_PARITY_POST_PASS=0` turns the post-pass off. A selection file that
/// cannot be read only warns, because the parity report must never stop the
/// determinism cells. An invalid `E2E_PARITY_SELECT` or `E2E_PARITY_POST_PASS`
/// is a usage error, refused before any cell runs or any output is created.
fn parity_scope(
    root: &Path,
    manifests: &ManifestSet,
    planned: &BTreeSet<(String, String)>,
) -> BTreeSet<ParityCellId> {
    match std::env::var(parity::PARITY_POST_PASS_ENV) {
        Err(std::env::VarError::NotPresent) => {}
        Ok(value) if value == "1" => {}
        Ok(value) if value == "0" => return BTreeSet::new(),
        Ok(value) => fail(format!(
            "{} must be 0 or 1, got {value:?}",
            parity::PARITY_POST_PASS_ENV
        )),
        Err(std::env::VarError::NotUnicode(_)) => {
            fail(format!("{} must be 0 or 1", parity::PARITY_POST_PASS_ENV))
        }
    }
    let explicit = std::env::var(parity::PARITY_SELECT_ENV).ok();
    match parity::resolve_scope(root, manifests, explicit.as_deref(), planned) {
        Ok(scope) => {
            for warning in &scope.warnings {
                eprintln!("test-harness: {warning}");
            }
            scope.cells
        }
        Err(error) => fail(format!("{}: {error}", parity::PARITY_SELECT_ENV)),
    }
}

/// Run the parity post-pass over this process's rows once `results.jsonl`,
/// JUnit and `summary.json` are final. It writes only `parity.jsonl`,
/// `parity.status.json` and `parity/` beside them, inside the enclosing
/// dagrun step's wall bound, and returns nothing: an error, or even a panic,
/// is reported and the exit status stays what determinism made it. With no
/// cell in scope it only removes an earlier run's parity outputs.
fn report_parity(
    scope: &BTreeSet<ParityCellId>,
    context: &RunContext,
    results_path: &Path,
    capacity: ScheduledWorkerCapacity,
    attempt_results: &[Vec<CellResult>],
) {
    let Some(artifacts) = results_path.parent() else {
        eprintln!("test-harness: parity post-pass skipped: results path has no directory");
        return;
    };
    let mut config = parity::PostPassConfig::new(
        artifacts,
        &context.hermit_bin,
        &context.run_id,
        &context.source_sha,
    );
    config.jobs = capacity.workers_for(scope.len());
    config.outer_deadline = parity::dagrun_step_deadline();
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

fn parse_parity_compare(values: Vec<String>) -> ParityCompareRequest {
    if values.iter().any(|value| is_help_flag(value)) {
        println!("{PARITY_HELP}");
        std::process::exit(0);
    }
    let mut values = values.into_iter();
    match values.next().as_deref() {
        Some("compare") => {}
        Some(other) => fail(format!(
            "unknown parity command {other:?}; expected `parity compare`"
        )),
        None => fail("parity requires a command; try `test-harness parity --help`"),
    }
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
    let hermit_bin =
        hermit_manifest_plan::runner::resolve_hermit_bin(root, std::env::var_os("HERMIT_BIN"));
    let mut config =
        parity::PostPassConfig::new(&request.artifacts, &hermit_bin, &run_id, &hermit_sha)
            .writing_below(&request.artifacts.join(PARITY_COMPARE_DIR));
    if let Some(output) = &request.output {
        config.output = output.clone();
    }
    config.jobs = request.jobs;
    config.outer_deadline = parity::dagrun_step_deadline();
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use hermit_manifest_plan::runner::FailureClass;
    use hermit_manifest_plan::runner::ManifestSet;
    use hermit_manifest_plan::runner::ScheduledWorkerCapacity;

    use super::CellResult;
    use super::DEFAULT_BUILD_JOBS;
    use super::DEFAULT_VALIDATE_AUDIT_JOBS;
    use super::EXPECTED_PLAN_SCHEMA;
    use super::HostCapability;
    use super::HostCapabilityVerdict;
    use super::PINNED_COMMAND_PREFIX;
    use super::PINNED_COMMAND_SEPARATOR;
    use super::PREBUILT_COMMAND_PREFIX;
    use super::TestResults;
    use super::accumulate_cell_cpu_usage;
    use super::audit_privileged_unboxed_guard;
    use super::audit_run_dag_workflow_runner;
    use super::audit_validation_levels_policy;
    use super::build_worker_capacity;
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
                "liteinst": "Not selected by this control"
            }});
            fs::write(manifests.join("parity.yaml"), serde_json::to_vec(&json!({
                "schema": 3, "bucket": "parity", "test": [{
                    "id": "parity/mixed", "description": "Native mixed routing control",
                    "lane": "portable", "occasional": false, "direct": ["/bin/true"],
                    "observation": {"status": true, "stdout": true, "stderr": true},
                    "modes": {
                        "verify": {"ci": true, "backends_enabled": ["ptrace", "kvm"],
                            "backends_disabled": {"dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected"}},
                        "naked": {"ci": false, "backends_enabled": [],
                            "backends_disabled": {"native": "Not selected by this CI control"}},
                        "chaos": disabled, "replay": disabled,
                        "custom": {"ci": true, "backends_enabled": ["kvm"],
                            "backends_disabled": {"ptrace": "Not selected", "dbt": "Not selected", "sabre": "Not selected", "liteinst": "Not selected"},
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
            let rows = fs::read_to_string(path.join("results.jsonl"))
                .unwrap()
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
                assert!(row.backend_parity.is_none(), "{scenario}");
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
            // The single-log `log-diff` call is the ptrace cell's own golden
            // normalization, part of its ordinary verification. A two-log
            // comparison would be recorded as "compare".
            assert_eq!(calls.len(), 4, "{scenario}: {calls:#?}");
            assert_eq!(
                kinds,
                BTreeSet::from([
                    "custom:kvm:false".to_string(),
                    "normalize:-:false".to_string(),
                    "run:kvm:false".to_string(),
                    "run:ptrace:false".to_string(),
                ]),
                "{scenario}: no ptrace reference run and no log-diff comparison"
            );
            let normalized = calls
                .iter()
                .find(|call| call["kind"] == "normalize")
                .unwrap();
            assert!(
                normalized["argv"][1]
                    .as_str()
                    .unwrap()
                    .contains("/parity-mixed-verify-ptrace/"),
                "{scenario}: only the ptrace cell's own log is normalized: {normalized}"
            );
        }
    }

    /// The parity post-pass of <https://github.com/rrnewton/hermit/issues/3301>
    /// reads only what determinism left behind. With no parity cell, with
    /// five, and with a planted mutation that makes every compared candidate
    /// diverge from ptrace, the harness exits the same way and writes the same
    /// `results.jsonl`, JUnit and `summary.json` (apart from wall and CPU
    /// timings), and it runs the same guest executions: the only extra
    /// processes are one `hermit log-diff` per measurable cell. That holds
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
            "liteinst": "Not selected by this control"
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
                    json!({"sabre": "Not selected"})),
                test("parity/beta", json!(["ptrace", "kvm", "liteinst"]),
                    json!({"dbt": "Not selected", "sabre": "Not selected"})),
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
        // One fake hermit: `run` is a guest execution and is counted; the
        // single-log `log-diff` is ptrace golden normalization; a two-log
        // `log-diff` is a parity comparison. The scenario is a list of words:
        // parity/beta's liteinst cell fails determinism unless it has `pass`,
        // and `mutated` changes every candidate's second detcore message.
        let hermit = fixture.join("hermit");
        // This fake starts about 150 times per test run, so it imports only
        // `os`. `-IS` skips the site-packages scan at every start, the
        // `--help` and version probes exit before any import, and each
        // invocation record is written as the literal line `json.dumps`
        // produces for it: the cell directory names are ASCII slugs with no
        // quote or backslash. Importing json and pathlib cost more than the
        // rest of the script. The log-diff stand-in starts with `-IS` too.
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
                .env("E2E_KERNEL_VERSION", "native-control");
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

        let (none, _, none_stdout, _, none_calls) = child("none", "shared", true, None, None, &[]);
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
                2,
                "E2E_KEEP_VERIFY_LOGS normalizes each ptrace golden"
            );
        }
        assert_eq!(count(&none_calls, "compare"), 0);
        assert!(!none.join("parity.jsonl").exists());
        assert!(!none.join("parity").exists());
        assert!(
            !none_stdout.contains("test-harness: parity:"),
            "{none_stdout}"
        );
        // alpha@kvm, alpha@liteinst and beta@kvm are measurable; dbt and a
        // candidate that failed determinism are not compared.
        assert_eq!(count(&five_calls, "compare"), 3);
        assert_eq!(count(&mutated_calls, "compare"), 3);
        assert!(
            five_stdout.contains("test-harness: parity: 5 cell(s)"),
            "{five_stdout}"
        );
        assert!(
            five_stdout.contains(
                "mean credit 1.0000 over 3 measured with equal inputs; none measured with \
                 unequal inputs"
            ),
            "{five_stdout}"
        );
        assert!(
            mutated_stdout.contains("3 log-diff comparison(s), 0 guest runs"),
            "{mutated_stdout}"
        );
        assert_eq!(
            verdicts(&five_records),
            [
                "parity/alpha@dbt=InputsNotEqualized",
                "parity/alpha@kvm=Matched",
                "parity/alpha@liteinst=Matched",
                "parity/beta@kvm=Matched",
                "parity/beta@liteinst=Unavailable",
            ]
        );
        assert!(
            five_records[4]
                .reason
                .as_deref()
                .unwrap()
                .contains("failed determinism"),
            "{:?}",
            five_records[4]
        );
        assert_eq!(
            verdicts(&mutated_records),
            [
                "parity/alpha@dbt=InputsNotEqualized",
                "parity/alpha@kvm=Diverged",
                "parity/alpha@liteinst=Diverged",
                "parity/beta@kvm=Diverged",
                "parity/beta@liteinst=Unavailable",
            ]
        );
        // The harness launched every kvm and liteinst verify cell, and their
        // ptrace reference, with the equalized guest inputs, so each measured
        // comparison is clean credit. dbt cannot be given them, and a cell
        // refused before comparing is not shown to have them.
        for records in [&five_records, &mutated_records] {
            for (index, record) in records.iter().enumerate() {
                record.validate().unwrap();
                let measured = (1..4).contains(&index);
                assert_eq!(record.inputs_equalized, measured, "{record:?}");
                assert_eq!(record.unequalized_credit, None, "{record:?}");
                if !measured {
                    assert_eq!(record.credit, None, "{record:?}");
                }
            }
        }
        for record in &five_records[1..4] {
            assert_eq!(record.credit, Some(1.0), "{record:?}");
        }
        for record in &mutated_records[1..4] {
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
        assert_eq!(five_status.cells, 5);
        assert!(five.join("parity/logdiff").is_dir());
        let (again, _, _, _, again_calls) = child("five", "shared", true, None, None, &[]);
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
            child("pass-off", "pass", true, None, None, &[]);
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
        assert_eq!(count(&pass_calls, "compare"), 4);
        assert_eq!(
            verdicts(&records(&pass.join("parity.jsonl"))),
            [
                "parity/alpha@dbt=InputsNotEqualized",
                "parity/alpha@kvm=Diverged",
                "parity/alpha@liteinst=Diverged",
                "parity/beta@kvm=Diverged",
                "parity/beta@liteinst=Diverged",
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
        assert_eq!(
            verdicts(&expired_records),
            [
                "parity/alpha@dbt=InputsNotEqualized",
                "parity/alpha@kvm=Unavailable",
                "parity/alpha@liteinst=Unavailable",
                "parity/beta@kvm=Unavailable",
                "parity/beta@liteinst=Unavailable",
            ]
        );
        for record in &expired_records[1..] {
            let reason = record.reason.as_deref().unwrap();
            assert!(
                reason.contains("the enclosing dagrun step's unknown wall bound")
                    && reason.contains("ran out before this comparison started"),
                "{record:?}"
            );
        }

        // Without E2E_KEEP_VERIFY_LOGS, the selection closure's logs are
        // still retained, the golden and its sidecar are written, and nothing
        // is normalized. Against the same run with parity off, every row is
        // the same apart from timings and the `--keep-logs --verify-log-dir
        // <dir>` that retains a compared cell's logs, which is in its argv,
        // effective_args and shell_command and in each attempt's argv and
        // shell_command.
        let (retained_off, _, retained_off_stdout, _, retained_off_calls) =
            child("retained-off", "shared", false, None, None, &[]);
        let (retained, _, _, _, retained_calls) =
            child("retained", "shared", false, Some(SELECT), None, &[]);
        for calls in [&retained_off_calls, &retained_calls] {
            assert_eq!(guest_runs(calls), expected_runs);
            assert_eq!(count(calls, "normalize"), 0);
        }
        assert_eq!(count(&retained_off_calls, "compare"), 0);
        assert_eq!(count(&retained_calls, "compare"), 3);
        assert!(!retained_off.join("parity.jsonl").exists());
        let retained_off_outputs = without_timings(&retained_off);
        assert_eq!(retained_off_outputs.3, baseline.3, "{retained_off_stdout}");
        assert_eq!(without_kept_logs(retained_off_outputs.clone()).1, 0);
        let (retained_outputs, kept_rows) = without_kept_logs(without_timings(&retained));
        // Seven rows keep logs: every row but alpha@dbt's, beta@liteinst's
        // twice.
        assert_eq!(kept_rows, 7);
        assert_eq!(
            retained_outputs, retained_off_outputs,
            "retaining the parity closure's logs changed a determinism output"
        );
        for row in rows(&retained) {
            let kept = row.argv.iter().any(|arg| arg == "--verify-log-dir");
            let backend = row.backend.as_deref().unwrap();
            assert_eq!(
                kept,
                backend != "dbt",
                "{} {backend}: {:?}",
                row.test,
                row.argv
            );
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
    /// none. `parse` exits the process on a repeated `--exclude-backend`, so
    /// the flag is counted before parsing to name the offending step.
    #[test]
    fn committed_hosted_portable_harness_commands_exclude_each_backend_once() {
        use hermit_manifest_plan::validation_dag::HOSTED_PORTABLE_EXCLUDED_BACKENDS;
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let mut hosted = 0;
        let mut local = 0;
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
        // type's bucket, which also runs on the host.
        assert_eq!((pinned, direct), (19, 18));
    }

    /// With the committed parity selection, the full profile's harness
    /// nodes together report each selected cell exactly once: a node reports
    /// a cell when it plans either verify side, and exactly one node plans
    /// each side. An `E2E_PARITY_SELECT` naming every applicable cell is
    /// reported the same way, apart from the cells no full node plans a side
    /// of, which every node drops with a warning.
    #[test]
    fn the_full_profile_reports_each_selected_parity_cell_exactly_once() {
        use hermit_manifest_plan::parity;
        let root = super::root(None);
        let manifests = ManifestSet::load(&root).unwrap();
        let committed = dagrun::dag_from_json(include_str!("../../../dag/validate.json"))
            .expect("actual committed graph");
        let full = dagrun::select_steps_by_labels(&committed, &["full".into()])
            .expect("actual full selection");
        let matrix = parity::ParityMatrix::derive(&manifests).unwrap();
        let selection = parity::ParitySelection::load(&root, &matrix).unwrap().cells;
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
        let lines = reported.values().map(Vec::len).sum::<usize>();
        assert_eq!((selection.len(), lines), (194, 194));
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
        assert_eq!(
            (applicable.len(), explicitly.len(), unreported.len()),
            (628, 624, 4),
            "{unreported:?}"
        );
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
        assert_eq!(kvm.len(), 244);
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
            host_inapplicable_reason(&requires, Some("ptrace"), &absent).unwrap();
        assert_eq!(capabilities, ["cpuid-faulting"]);
        assert!(reason.contains("NOT RUN, NOT a pass, no coverage"));
        assert!(reason.contains("planted absence"));

        let undeclared = vec!["linux".to_string(), "ptrace".to_string()];
        assert!(host_inapplicable_reason(&undeclared, Some("ptrace"), &absent).is_none());

        let present = BTreeMap::from([(
            HostCapability::CpuidFaulting,
            HostCapabilityVerdict {
                present: true,
                evidence: "planted presence".into(),
            },
        )]);
        assert!(host_inapplicable_reason(&requires, Some("ptrace"), &present).is_none());
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
            host_inapplicable_reason(&requires, Some("kvm"), &verdicts(false)).unwrap();
        assert_eq!(capabilities, ["kvm"]);
        assert!(reason.contains("errno=2"), "{reason}");
        assert!(host_inapplicable_reason(&requires, Some("ptrace"), &verdicts(false)).is_none());
        assert!(host_inapplicable_reason(&requires, None, &verdicts(false)).is_none());
        assert!(host_inapplicable_reason(&requires, Some("kvm"), &verdicts(true)).is_none());
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
                "portable preflight job 600s must cover its 3900s constructed DAG critical path plus at least 420s"
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
                "privileged job 2640s must cover 2610s of explicit inner step budgets plus at least 300s"
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
            "check.backend_parity_suites",
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
            "test.liteinst_strict",
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
        // (25 to 23), and test.dbt_parity left the dbt-parity shard in slice
        // S13 of https://github.com/rrnewton/hermit/issues/3301 (23 to 22).
        assert_eq!(physical_rows, 22);
        assert_eq!(resolved.len(), 22);
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
        let all = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
        let test = |id: &str, diagnostic: bool| {
            let mut verify = json!({
                "ci": true,
                "backends_enabled": ["ptrace"],
                "backends_disabled": {
                    "dbt": "fixture", "kvm": "fixture", "sabre": "fixture", "liteinst": "fixture"
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
                "liteinst": "This control executes native commands only"
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
}
