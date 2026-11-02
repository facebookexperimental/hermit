#!/usr/bin/env -S rust-script --force
//! Run a Hermit test suite as a validation node on a backend that does not pass all of it yet,
//! held to the suite's known-gap ledger, and hold the ledger to its history.
//!
//! A ledger (for example `hermit-cli/tests/deadline_slice_cap.kvm-gaps.tsv`) lists the suite's
//! known gaps on the node's backend: the tests that do not pass there yet, each with its tracking
//! issue and the way it must fail, or `skipped` when the node does not run it at all. Every other
//! test of the suite is required to pass, so a new test is required by default.
//! `ci/manifest-plan/src/known_gaps.rs` defines the format, the declarations of the nodes that
//! have a ledger, and the rules. The coordinator's four conditions hold as follows, and a fifth
//! covers the skipped rows:
//!
//! 1. A gap row never counts as a pass. The node runs it. A failure that matches the row's
//!    outcome and signature is a non-blocking diagnostic failure (`KNOWN-KVM-GAP`, dagrun's
//!    structured-result schema 4), counted apart from passes and failures. A failure that does
//!    not match is reported loudly (`KVM-GAP-WRONG-FAILURE`) and fails the node.
//! 2. Every row names its tracking issue as a full URL.
//! 3. The ledger can only shrink. A row that passes fails the node with "now passing, promote to
//!    required" (`KVM-GAP-NOW-PASSING`); it is recorded as a blocking failure, so even then it is
//!    not counted as a pass. `history`, which `make lint-checks` runs, refuses a row added or
//!    changed after the ledger's introduction unless its approval URL is bound to it by a
//!    commit-message line.
//! 4. A required test that fails, fails the node (`KVM-REQUIRED-FAIL`). Moving it into the ledger
//!    is adding a row. Deleting it, or `#[ignore]`-ing it, needs an approval line too.
//! 5. A skipped row is not run on the backend. The node prints `KVM-GAP-SKIPPED` for it and
//!    leaves it out of the run by name, with a Nextest filter expression built from the ledger,
//!    and it refuses a run that leaves out any other test or keeps a skipped one in. A skipped row
//!    is never a pass, a failure or a diagnostic; it is filtered out. Turning a row skipped is
//!    changing it, so it needs a bound approval; deleting a skipped row makes its test required.
//!
//! Subcommands:
//! - `check` applies the static rules to a ledger and its suite's source. This script's unit
//!   tests also apply them to every declared ledger.
//! - `history` applies them too, and then the history rules
//!   ([`hermit_manifest_plan::known_gaps::judge_history`]).
//! - `run` is the node. It reads its ledger from `HERMIT_KNOWN_GAP_LEDGER`, which the DAG writes
//!   from the declaration, and lists the suite with the prepared Nextest runner. It then runs every
//!   test of the suite but the skipped rows on the backend in one counted run, with no retry, and
//!   writes the node's structured results to `DAGRUN_TEST_COUNTS_PATH`. Beside them it keeps the
//!   counted run's CPU report (`.cpu.json`) and its event stream (`.events.jsonl`), which hold
//!   each failure's cause and output.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { version = "0.15.0", git = "https://github.com/rrnewton/agent-utils.git", rev = "9a55aa7d2d92435a382622de8ec2823ab0481645" }
//! hermit-manifest-plan = { path = "manifest-plan" }
//! serde_json = "1"
//! ```

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;

use dagrun::TestAttemptOutcome;
use dagrun::TestAttemptResult;
use dagrun::TestResult;
use dagrun::TestResults;
use hermit_manifest_plan::known_gaps;
use hermit_manifest_plan::known_gaps::CommitTrailers;
use hermit_manifest_plan::known_gaps::Declaration;
use hermit_manifest_plan::known_gaps::HistoryVerdict;
use hermit_manifest_plan::known_gaps::Outcome;
use hermit_manifest_plan::known_gaps::Row;
use hermit_manifest_plan::known_gaps::Version;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

/// Nextest's exit status when every test ran and at least one failed.
const NEXTEST_TEST_FAILURES: i32 = 100;

fn usage() -> &'static str {
    "usage:\n  \
     kvm-gap-ledger.rs check [LEDGER]\n      \
     Check LEDGER (a path from the repository root), or every declared ledger, against its\n      \
     suite's tests: the rules the node applies before it runs anything.\n  \
     kvm-gap-ledger.rs history [LEDGER]\n      \
     The same, then hold the ledger to its committed history: it may only shrink, and a row\n      \
     added or changed after its introduction carries an approval URL that a commit-message\n      \
     line `KVM-Gap-Approval: URL ID` binds to it. Needs the whole first-parent history since\n      \
     the ledger's introduction and a working tree whose ledger and suite match HEAD.\n  \
     kvm-gap-ledger.rs run -- NEXTEST_ARGS...\n      \
     The validation node. Reads its ledger from HERMIT_KNOWN_GAP_LEDGER, prints KVM-GAP-SKIPPED\n      \
     for each skipped row and leaves those tests out by name, lists the suite with NEXTEST_ARGS,\n      \
     runs every other test once on the declared backend (HERMIT_TEST_BACKEND,\n      \
     NEXTEST_RETRIES=0) through ci/run-nextest-counted.sh, prints KNOWN-KVM-GAP for each gap\n      \
     row that failed as its row says, and writes the node's schema-4 structured results to\n      \
     DAGRUN_TEST_COUNTS_PATH. NEXTEST_EXPECTED_EXECUTED is the number of required tests.\n      \
     NEXTEST_ARGS select the suite and carry no filter expression: the node chooses its tests.\n      \
     Exits 0 only when every required test passed, the backend-proof test proved the backend,\n      \
     and every gap row it ran failed as its row says.\n\n\
     Exit status: 0 pass, 1 refused (a rule broken, or the node failed), 2 could not decide."
}

// ---------------------------------------------------------------------------------------------
// The static rules.

/// A declared ledger's rows and its suite's tests, read from `root` (the working tree).
fn read_declared(
    root: &Path,
    declaration: &Declaration,
) -> Result<(Vec<Row>, BTreeMap<String, bool>), String> {
    let text = fs::read_to_string(root.join(declaration.ledger))
        .map_err(|error| format!("cannot read {}: {error}", declaration.ledger))?;
    let rows =
        known_gaps::parse(&text).map_err(|error| format!("{}: {error}", declaration.ledger))?;
    let source = fs::read_to_string(root.join(declaration.suite_source))
        .map_err(|error| format!("cannot read {}: {error}", declaration.suite_source))?;
    let tests = known_gaps::suite_tests(&source)
        .map_err(|error| format!("{}: {error}", declaration.suite_source))?;
    Ok((rows, tests))
}

/// The static rules' violations, one indented line each, or `None`.
fn static_violations(
    declaration: &Declaration,
    rows: &[Row],
    tests: &BTreeMap<String, bool>,
) -> Option<String> {
    let errors = known_gaps::check(declaration, rows, tests);
    (!errors.is_empty()).then(|| {
        errors
            .iter()
            .map(|error| format!("  {error}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// The declarations a subcommand covers: the one whose ledger is `ledger`, or every one.
fn selected(ledger: Option<&str>) -> Result<Vec<&'static Declaration>, String> {
    match ledger {
        Some(ledger) => known_gaps::declaration_for_ledger(ledger)
            .map(|declaration| vec![declaration])
            .ok_or_else(|| {
                format!(
                    "{ledger} is not a declared known-gap ledger (ci/manifest-plan/src/known_gaps.rs DECLARATIONS)"
                )
            }),
        None => Ok(known_gaps::DECLARATIONS.iter().collect()),
    }
}

fn check(root: &Path, ledger: Option<&str>) -> Result<bool, String> {
    let mut ok = true;
    for declaration in selected(ledger)? {
        let (rows, tests) = read_declared(root, declaration)?;
        match static_violations(declaration, &rows, &tests) {
            None => println!(
                "kvm-gap-ledger: {}: {} gap row(s), {} required test(s); every static rule holds",
                declaration.ledger,
                rows.len(),
                known_gaps::required(declaration, &rows, &tests).len()
            ),
            Some(errors) => {
                ok = false;
                eprintln!(
                    "kvm-gap-ledger: {} breaks its rules:\n{errors}",
                    declaration.ledger
                );
            }
        }
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------------------------
// The history gate.

/// `git ARGS` in `root`, read-only: no optional locks, and no signature lines in log output.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "log.showSignature=false",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn git_text(root: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git(root, args)?).map_err(|error| format!("git output is not UTF-8: {error}"))
}

/// The text of `path` at `commit`, or `None` if the commit has no such file.
fn file_at(root: &Path, commit: &str, path: &str) -> Result<Option<String>, String> {
    let object = format!("{commit}:{path}");
    let exists = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "-e", &object])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !exists.success() {
        return Ok(None);
    }
    git_text(root, &["show", &object]).map(Some)
}

/// A ledger's history: where it was introduced, how many versions there are, and the verdict.
#[derive(Debug)]
struct History {
    introduction: String,
    versions: usize,
    verdict: HistoryVerdict,
}

/// Read the history the gate judges: every committed version of the ledger and the suite along
/// HEAD's first-parent history since the ledger's introduction, and every commit message's
/// approval lines. Could-not-determine is an error, never a pass.
fn history_of(root: &Path, declaration: &Declaration) -> Result<History, String> {
    if git_text(root, &["rev-parse", "--is-shallow-repository"])?.trim() != "false" {
        return Err(
            "the repository is shallow, so the ledger's history since its introduction may be missing; \
             fetch the whole history (git fetch --unshallow) and run again"
                .into(),
        );
    }
    for path in [declaration.ledger, declaration.suite_source] {
        let committed = file_at(root, "HEAD", path)?;
        let working = fs::read_to_string(root.join(path)).ok();
        if committed != working {
            return Err(format!(
                "{path} differs from HEAD's; the history gate judges commits, so commit it first"
            ));
        }
    }
    let added = git_text(
        root,
        &[
            "log",
            "--first-parent",
            "--diff-filter=A",
            "--format=%H",
            "--reverse",
            "HEAD",
            "--",
            declaration.ledger,
        ],
    )?;
    let introduction = added
        .lines()
        .next()
        .ok_or_else(|| {
            format!(
                "{} is in no commit of HEAD's first-parent history",
                declaration.ledger
            )
        })?
        .to_string();
    let has_parent = git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{introduction}^"),
        ],
    )
    .is_ok();
    let range = if has_parent {
        format!("{introduction}^..HEAD")
    } else {
        "HEAD".to_string()
    };
    let commits = git_text(root, &["rev-list", "--first-parent", "--reverse", &range])?;
    let positions = commits
        .lines()
        .enumerate()
        .map(|(position, commit)| (commit.to_string(), position))
        .collect::<BTreeMap<_, _>>();
    if positions.get(&introduction) != Some(&0) {
        return Err(format!(
            "the ledger's introduction {introduction} does not start HEAD's first-parent history since it; the history walk is inconsistent"
        ));
    }
    let changed = git_text(
        root,
        &[
            "log",
            "--first-parent",
            "--format=%H",
            "--reverse",
            &range,
            "--",
            declaration.ledger,
            declaration.suite_source,
        ],
    )?;
    let mut judged = vec![introduction.clone()];
    judged.extend(
        changed
            .lines()
            .filter(|commit| *commit != introduction)
            .map(str::to_string),
    );
    let mut versions = Vec::with_capacity(judged.len());
    for commit in judged {
        let position = *positions
            .get(&commit)
            .ok_or_else(|| format!("{commit} is not on HEAD's first-parent history"))?;
        let rows = match file_at(root, &commit, declaration.ledger)? {
            Some(text) => known_gaps::parse(&text)
                .map_err(|error| format!("{} at {commit}: {error}", declaration.ledger))?,
            None => Vec::new(),
        };
        let source = file_at(root, &commit, declaration.suite_source)?
            .ok_or_else(|| format!("{} is missing at {commit}", declaration.suite_source))?;
        let tests = known_gaps::suite_tests(&source)
            .map_err(|error| format!("{} at {commit}: {error}", declaration.suite_source))?;
        versions.push(Version {
            position,
            commit,
            rows,
            tests,
        });
    }
    versions.sort_by_key(|version| version.position);
    let log = git(
        root,
        &["log", "--first-parent", "--format=%H%x00%B%x1e", &range],
    )?;
    let log = String::from_utf8_lossy(&log);
    let mut trailers = Vec::new();
    for record in log.split('\x1e') {
        let record = record.trim_start_matches('\n');
        let Some((commit, message)) = record.split_once('\0') else {
            continue;
        };
        let found = known_gaps::parse_trailers(message);
        if found.is_empty() {
            continue;
        }
        let position = *positions
            .get(commit)
            .ok_or_else(|| format!("{commit} is not on HEAD's first-parent history"))?;
        trailers.push(CommitTrailers {
            position,
            commit: commit.to_string(),
            trailers: found,
        });
    }
    let count = versions.len();
    Ok(History {
        introduction,
        versions: count,
        verdict: known_gaps::judge_history(declaration, &versions, &trailers),
    })
}

fn history(root: &Path, ledger: Option<&str>) -> Result<bool, String> {
    let mut ok = true;
    for declaration in selected(ledger)? {
        let (rows, tests) = read_declared(root, declaration)?;
        if let Some(errors) = static_violations(declaration, &rows, &tests) {
            ok = false;
            eprintln!(
                "kvm-gap-ledger: {} breaks its rules:\n{errors}",
                declaration.ledger
            );
            continue;
        }
        let history = history_of(root, declaration)?;
        let verdict = &history.verdict;
        if verdict.violations.is_empty() {
            println!(
                "kvm-gap-ledger: {}: history holds: introduced at {}, {} committed version(s); {} baseline row(s), {} approved row(s), {} required test(s)",
                declaration.ledger,
                &history.introduction[..12],
                history.versions,
                verdict.baseline_rows,
                verdict.approved_rows,
                verdict.required
            );
        } else {
            ok = false;
            eprintln!(
                "KNOWN-GAP-LEDGER-HISTORY-REFUSED {} (introduced at {}): the ledger may only shrink, and what grows it needs a bound approval:\n{}",
                declaration.ledger,
                &history.introduction[..12],
                verdict
                    .violations
                    .iter()
                    .map(|violation| format!("  {violation}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
    Ok(ok)
}

// ---------------------------------------------------------------------------------------------
// The node.

/// The tests a run of the suite executes, as structured-result ids, from Nextest's `list
/// --message-format json` of the declared binary.
///
/// That list is checked test by test against the suite's source (`tests`, from
/// [`known_gaps::suite_tests`]): the same test names, each ignored on both sides or on neither, and
/// every test that is not ignored selected, except the ledger's skipped rows (`skipped`, test
/// names), which must each be left out. A test the source parser does not see (one a macro
/// generates, say), or one it reads differently, refuses the run. So the tests the run executes are
/// exactly the ones the ledger, its required count and its history judge, and none can stand in
/// for another.
fn listed_population(
    list: &serde_json::Value,
    declaration: &Declaration,
    tests: &BTreeMap<String, bool>,
    skipped: &BTreeSet<String>,
) -> Result<BTreeSet<String>, String> {
    let binary = declaration
        .id_prefix
        .strip_suffix('$')
        .ok_or_else(|| format!("{} does not end with $", declaration.id_prefix))?;
    let suite = list
        .get("rust-suites")
        .and_then(serde_json::Value::as_object)
        .ok_or("the Nextest list has no rust-suites object")?
        .get(binary)
        .ok_or_else(|| format!("the Nextest list has no test binary {binary}"))?;
    let cases = suite
        .get("testcases")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{binary} has no testcases object"))?;
    let mut listed = BTreeMap::new();
    for (test, case) in cases {
        let selected = case
            .pointer("/filter-match/status")
            .and_then(serde_json::Value::as_str)
            == Some("matches");
        let ignored = case
            .get("ignored")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| format!("{binary} {test} has no boolean ignored field"))?;
        listed.insert(test.clone(), (ignored, selected));
    }
    let names = |names: Vec<&String>| {
        if names.is_empty() {
            "none".to_string()
        } else {
            names
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    let only_listed = listed
        .keys()
        .filter(|test| !tests.contains_key(*test))
        .collect::<Vec<_>>();
    let only_declared = tests
        .keys()
        .filter(|test| !listed.contains_key(*test))
        .collect::<Vec<_>>();
    let ignored_apart = tests
        .iter()
        .filter(|(test, ignored)| {
            listed
                .get(*test)
                .is_some_and(|(listed, _)| listed != *ignored)
        })
        .map(|(test, _)| test)
        .collect::<Vec<_>>();
    let filtered = listed
        .iter()
        .filter(|(test, (ignored, selected))| !ignored && !selected && !skipped.contains(*test))
        .map(|(test, _)| test)
        .collect::<Vec<_>>();
    let skipped_kept = listed
        .iter()
        .filter(|(test, (_, selected))| *selected && skipped.contains(*test))
        .map(|(test, _)| test)
        .collect::<Vec<_>>();
    if !(only_listed.is_empty()
        && only_declared.is_empty()
        && ignored_apart.is_empty()
        && filtered.is_empty()
        && skipped_kept.is_empty())
    {
        return Err(format!(
            "Nextest's list of {binary} and its source {} disagree, so the run could execute other tests than the ledger judges. Listed but not declared: {}. Declared but not listed: {}. Ignored on one side only: {}. Not ignored but filtered out: {}. Skipped rows selected anyway: {}",
            declaration.suite_source,
            names(only_listed),
            names(only_declared),
            names(ignored_apart),
            names(filtered),
            names(skipped_kept)
        ));
    }
    let population = listed
        .iter()
        .filter(|(_, (ignored, selected))| !ignored && *selected)
        .map(|(test, _)| format!("{}{test}", declaration.id_prefix))
        .collect::<BTreeSet<_>>();
    if population.is_empty() {
        return Err(format!("the Nextest list selects no test of {binary}"));
    }
    Ok(population)
}

/// Check the ledger against the population the run will execute and the node's pinned number of
/// required tests; return the required tests. The backend-proof test must be in the population:
/// without it nothing shows that the run was on the declared backend. Every gap row the node runs
/// must be in it, and no skipped row may be.
fn plan(
    declaration: &Declaration,
    rows: &[Row],
    population: &BTreeSet<String>,
    expected_required: usize,
) -> Result<BTreeSet<String>, String> {
    let proof = format!(
        "{}{}",
        declaration.id_prefix, declaration.backend_proof_test
    );
    if !population.contains(&proof) {
        return Err(format!(
            "the backend-proof test {proof} is not in the run's selection; without it nothing shows that the run was on --backend={}",
            declaration.backend
        ));
    }
    let (run, not_run): (Vec<&Row>, Vec<&Row>) = rows.iter().partition(|row| row.outcome.is_run());
    let ids = |rows: Vec<&Row>| {
        rows.into_iter()
            .map(|row| row.id.clone())
            .collect::<BTreeSet<_>>()
    };
    let (gaps, skipped) = (ids(run), ids(not_run));
    let unknown = gaps.difference(population).cloned().collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(format!(
            "gap rows name tests the run would not execute: {}",
            unknown.join(", ")
        ));
    }
    let kept = skipped
        .intersection(population)
        .cloned()
        .collect::<Vec<_>>();
    if !kept.is_empty() {
        return Err(format!(
            "skipped rows name tests the run would execute: {}",
            kept.join(", ")
        ));
    }
    let required = population
        .difference(&gaps)
        .cloned()
        .collect::<BTreeSet<_>>();
    if required.len() != expected_required {
        return Err(format!(
            "the run executes {} tests, {} of them gap rows, so {} are required ({} skipped rows are not run), but the node is pinned to {expected_required} (NEXTEST_EXPECTED_EXECUTED); a deleted row raises the pin, a new test raises it, and a new row needs a bound approval: update the pin with the change that moves it",
            population.len(),
            gaps.len(),
            required.len(),
            skipped.len()
        ));
    }
    Ok(required)
}

/// How a test's one attempt ended, from the counted run's CPU report.
#[derive(Clone, Debug, Eq, PartialEq)]
struct AttemptEnd {
    outcome: TestAttemptOutcome,
    /// What ended it, for a non-pass.
    detail: String,
}

/// Each test's attempt ends from a schema-3 CPU report, by structured-result id.
fn attempt_ends(report: &serde_json::Value) -> Result<BTreeMap<String, AttemptEnd>, String> {
    let attempts = report
        .get("attempts")
        .and_then(serde_json::Value::as_array)
        .ok_or("the CPU report has no attempts list")?;
    let mut ends = BTreeMap::new();
    for record in attempts {
        let field = |pointer: &str| {
            record
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("a CPU report attempt has no {pointer}: {record}"))
        };
        let number = |pointer: &str| {
            record
                .pointer(pointer)
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| format!("a CPU report attempt has no integer {pointer}: {record}"))
        };
        let id = format!(
            "{}${}",
            field("/identity/binary")?,
            field("/identity/test")?
        );
        let end = match field("/completion/kind")? {
            "exit" => match number("/completion/code")? {
                0 => AttemptEnd {
                    outcome: TestAttemptOutcome::Passed,
                    detail: String::new(),
                },
                code => AttemptEnd {
                    outcome: TestAttemptOutcome::Failed,
                    detail: format!("exited with status {code}"),
                },
            },
            "signal" => AttemptEnd {
                outcome: TestAttemptOutcome::Failed,
                detail: format!("was killed by signal {}", number("/completion/signal")?),
            },
            "supervisor_signal" => AttemptEnd {
                outcome: TestAttemptOutcome::WallTimeout,
                detail: format!(
                    "was stopped by signal {} from Nextest: its wall-clock backstop, or a cancellation",
                    number("/completion/signal")?
                ),
            },
            "cpu_timeout" => AttemptEnd {
                outcome: TestAttemptOutcome::CpuTimeout,
                detail: format!(
                    "was stopped by its CPU budget: {:.1} s of CPU used against a budget of {:.1} s",
                    number("/completion/observed_cpu_usec")? as f64 / 1e6,
                    number("/completion/cpu_budget_usec")? as f64 / 1e6
                ),
            },
            other => {
                return Err(format!(
                    "a CPU report attempt has completion kind {other:?}"
                ));
            }
        };
        if ends.insert(id.clone(), end).is_some() {
            return Err(format!(
                "the CPU report has more than one attempt of {id}, but this node runs every test once"
            ));
        }
    }
    Ok(ends)
}

/// Each failed test's captured output, by structured-result id, from the counted run's
/// libtest-json-plus event stream.
fn failure_outputs(events: &str) -> Result<BTreeMap<String, String>, String> {
    let mut outputs = BTreeMap::new();
    for line in events.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("an event is not JSON ({error}): {line}"))?;
        if event.get("type").and_then(serde_json::Value::as_str) != Some("test")
            || event.get("event").and_then(serde_json::Value::as_str) != Some("failed")
        {
            continue;
        }
        let name = event
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("a failed event has no name: {line}"))?;
        let output = event
            .get("stdout")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        outputs.insert(name.to_string(), output.to_string());
    }
    Ok(outputs)
}

/// What a run produced, beside its exit status.
struct Evidence<'a> {
    /// The counted run's schema-2 report of every test.
    all: &'a TestResults,
    /// The counted run's exit status.
    status: i32,
    ends: &'a BTreeMap<String, AttemptEnd>,
    outputs: &'a BTreeMap<String, String>,
    /// The backend-proof test's lines, or `None` when it wrote no file.
    proof: Option<&'a [String]>,
}

/// What the node reports for one finished run.
#[derive(Debug)]
struct Verdict {
    /// The node's structured results, schema 4.
    results: TestResults,
    /// One line per gap row, required failure and summary, for the node's log.
    lines: Vec<String>,
    /// Every required test passed, the backend was proved, and every gap row failed as its row
    /// says.
    ok: bool,
}

fn attempt(outcome: TestAttemptOutcome, detail: String) -> Result<Vec<TestAttemptResult>, String> {
    let detail = (outcome != TestAttemptOutcome::Passed).then_some(detail);
    Ok(vec![TestAttemptResult::new(1, outcome, detail)?])
}

/// Judge a run of every test of the suite against the ledger.
///
/// A required test keeps its result, with its attempt's cause. A gap row that failed as its row
/// says becomes a diagnostic failure, `KNOWN-KVM-GAP`: failing, known, never a pass. A gap row
/// that failed another way, or passed, becomes a blocking failure. The backend-proof test passes
/// only if it proved the declared backend. The run must report every test of the population
/// once, with one attempt, and its status must be 0 (all passed) or Nextest's
/// [`NEXTEST_TEST_FAILURES`] and agree with the report; anything else is refused, so the node
/// writes no results.
fn judge(
    declaration: &Declaration,
    rows: &[Row],
    population: &BTreeSet<String>,
    evidence: &Evidence,
) -> Result<Verdict, String> {
    let results = evidence
        .all
        .results
        .as_ref()
        .ok_or("the counted run wrote count-only results")?;
    let mut by_id = BTreeMap::new();
    for result in results {
        if result.attempts != 1 {
            return Err(format!(
                "the counted run reports {} attempts of {}, but this node runs every test exactly once (NEXTEST_RETRIES=0): a retry would let a required test pass on a second try",
                result.attempts, result.id
            ));
        }
        if by_id.insert(result.id.as_str(), result).is_some() {
            return Err(format!("the counted run reports {} twice", result.id));
        }
    }
    let proof_id = format!(
        "{}{}",
        declaration.id_prefix, declaration.backend_proof_test
    );
    if !by_id.contains_key(proof_id.as_str()) {
        return Err(format!(
            "the counted run reports no result for the backend-proof test {proof_id}; without it nothing shows that the run was on --backend={}",
            declaration.backend
        ));
    }
    let reported = by_id
        .keys()
        .map(|id| id.to_string())
        .collect::<BTreeSet<_>>();
    if &reported != population || evidence.all.executed_tests != population.len() as u64 {
        return Err(format!(
            "the counted run reports {} executed test(s) ({}) for a population of {}; missing: {:?}; unexpected: {:?}",
            evidence.all.executed_tests,
            reported.len(),
            population.len(),
            population.difference(&reported).collect::<Vec<_>>(),
            reported.difference(population).collect::<Vec<_>>()
        ));
    }
    let failures = results.iter().filter(|result| !result.passed).count();
    match evidence.status {
        0 if failures == 0 => {}
        NEXTEST_TEST_FAILURES if failures > 0 => {}
        status => {
            return Err(format!(
                "the counted run exited {status} with {failures} failing test(s) in its report; only 0 with none or {NEXTEST_TEST_FAILURES} with some is a finished run"
            ));
        }
    }
    // A skipped row has no result: the run left it out, and the population check above refuses a
    // result for any test outside the population.
    let gaps = rows
        .iter()
        .filter(|row| row.outcome.is_run())
        .map(|row| (row.id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    let skipped = rows.len() - gaps.len();
    let ledger = declaration.ledger;
    let backend = declaration.backend;
    let mut out = Vec::with_capacity(results.len());
    let mut lines = Vec::new();
    let (mut required_passed, mut required_failed) = (0usize, 0usize);
    let (mut gaps_known, mut gaps_wrong, mut gaps_passing) = (0usize, 0usize, 0usize);
    for (id, result) in by_id {
        let end = evidence.ends.get(id).ok_or_else(|| {
            format!("the CPU report has no attempt of {id}, which the counted run reports")
        })?;
        if (end.outcome == TestAttemptOutcome::Passed) != result.passed {
            return Err(format!(
                "the CPU report says {id} {}, the counted run says it {}",
                if end.outcome == TestAttemptOutcome::Passed {
                    "passed"
                } else {
                    "did not pass"
                },
                if result.passed { "passed" } else { "failed" }
            ));
        }
        let id = id.to_string();
        match gaps.get(id.as_str()) {
            None if result.passed && id == proof_id => {
                let proved = evidence.proof.is_some_and(|lines| {
                    !lines.is_empty() && lines.iter().all(|line| line == backend)
                });
                if proved {
                    required_passed += 1;
                    out.push(TestResult::with_attempt_results(
                        id,
                        true,
                        attempt(TestAttemptOutcome::Passed, String::new())?,
                    )?);
                } else {
                    required_failed += 1;
                    let seen = match evidence.proof {
                        None => "no backend proof at all".to_string(),
                        Some(lines) => format!("the backend proof {lines:?}"),
                    };
                    lines.push(format!(
                        "KVM-BACKEND-UNPROVEN {id}: it passed, but left {seen} where its verified runs must each record {backend:?}; the suite may not have run on --backend={backend}"
                    ));
                    out.push(TestResult::with_attempt_results(
                        id,
                        false,
                        attempt(
                            TestAttemptOutcome::Failed,
                            format!(
                                "passed, but left {seen} where each verified run must record {backend:?} ({}): the suite may not have run on --backend={backend}",
                                known_gaps::BACKEND_PROOF_ENV
                            ),
                        )?,
                    )?);
                }
            }
            None if result.passed => {
                required_passed += 1;
                out.push(TestResult::with_attempt_results(
                    id,
                    true,
                    attempt(TestAttemptOutcome::Passed, String::new())?,
                )?);
            }
            None => {
                required_failed += 1;
                lines.push(format!(
                    "KVM-REQUIRED-FAIL {id}: a required test failed on --backend={backend} ({}; it {}), so the node fails; it may join {ledger} only with a bound approval",
                    end.outcome.value(),
                    end.detail
                ));
                out.push(TestResult::with_attempt_results(
                    id,
                    false,
                    attempt(
                        end.outcome,
                        format!(
                            "required on --backend={backend}, and it {}; this node's log has Nextest's output",
                            end.detail
                        ),
                    )?,
                )?);
            }
            Some(row) if result.passed => {
                gaps_passing += 1;
                lines.push(format!(
                    "KVM-GAP-NOW-PASSING {id} {} {}: now passing, promote to required: delete its row from {ledger} and raise the node's pinned count",
                    row.gap, row.issue
                ));
                out.push(TestResult::with_attempt_results(
                    id,
                    false,
                    attempt(
                        TestAttemptOutcome::Failed,
                        format!(
                            "PASSED on --backend={backend}, but {ledger} lists it as a known gap: now passing, promote to required (a gap row is never counted as a pass)"
                        ),
                    )?,
                )?);
            }
            Some(row) => {
                let expected = match row.outcome {
                    Outcome::Failed => TestAttemptOutcome::Failed,
                    Outcome::CpuTimeout => TestAttemptOutcome::CpuTimeout,
                    // `gaps` holds only the rows the node runs.
                    Outcome::Skipped => {
                        return Err(format!(
                            "{id} is a skipped row, but the counted run reports a result for it"
                        ));
                    }
                };
                let output = evidence.outputs.get(&id).map(String::as_str).unwrap_or("");
                let signature_shown = row
                    .signature
                    .as_deref()
                    .is_none_or(|signature| known_gaps::signature_matches(signature, output));
                if end.outcome == expected && signature_shown {
                    gaps_known += 1;
                    lines.push(format!("KNOWN-KVM-GAP {id} {} {}", row.gap, row.issue));
                    let shown = match row.signature.as_deref() {
                        Some(signature) => format!(", and its output shows {signature:?}"),
                        None => String::new(),
                    };
                    out.push(TestResult::diagnostic_failure(
                        id,
                        attempt(
                            end.outcome,
                            format!(
                                "failed on --backend={backend} as its known gap ({}): it {}{shown}",
                                row.outcome.value(),
                                end.detail
                            ),
                        )?,
                        format!("KNOWN-KVM-GAP {} {}", row.gap, row.issue),
                    )?);
                } else {
                    gaps_wrong += 1;
                    let wanted = match row.signature.as_deref() {
                        Some(signature) => {
                            format!("{} with {signature:?} in its output", row.outcome.value())
                        }
                        None => row.outcome.value().to_string(),
                    };
                    let got = if end.outcome == expected {
                        format!(
                            "{}, but its output lacks the signature",
                            end.outcome.value()
                        )
                    } else {
                        format!("{}: it {}", end.outcome.value(), end.detail)
                    };
                    lines.push(format!(
                        "KVM-GAP-WRONG-FAILURE {id} {} {}: it failed, but not as its known gap: expected {wanted}, got {got}. A different failure is not the known gap, so the node fails",
                        row.gap, row.issue
                    ));
                    out.push(TestResult::with_attempt_results(
                        id,
                        false,
                        attempt(
                            end.outcome,
                            format!(
                                "failed on --backend={backend}, but not as its known gap ({} {}): expected {wanted}, got {got}",
                                row.gap, row.issue
                            ),
                        )?,
                    )?);
                }
            }
        }
    }
    let ok = required_failed == 0 && gaps_wrong == 0 && gaps_passing == 0;
    lines.push(format!(
        "{ledger} on --backend={backend}: {} required test(s), {required_passed} passed and {required_failed} failed; {} gap row(s) run, {gaps_known} failed as known (KNOWN-KVM-GAP, never counted as passes), {gaps_wrong} failed another way and {gaps_passing} passed; {skipped} skipped row(s) not run (KVM-GAP-SKIPPED); {}",
        required_passed + required_failed,
        gaps_known + gaps_wrong + gaps_passing,
        if ok { "PASS" } else { "FAIL" }
    ));
    let results = TestResults::diagnostic(
        evidence.all.executed_tests,
        evidence.all.filtered_tests,
        out,
    )?;
    Ok(Verdict { results, lines, ok })
}

/// The repository root: the working directory, which must hold `path`.
fn repository_root(path: &str) -> Result<PathBuf, String> {
    let root = env::current_dir().map_err(|error| format!("no working directory: {error}"))?;
    if !root.join(path).is_file() {
        return Err(format!(
            "run from the repository root: {} has no {path}",
            root.display()
        ));
    }
    Ok(root)
}

/// A command running one of the repository's rust-scripts. Official consumers name the
/// prepared runner directly, as ci/run-nextest-counted.sh does: a nested `/usr/bin/env
/// rust-script` may not resolve the repository wrapper inside the validation namespace.
fn rust_script(root: &Path, script: &str) -> Command {
    if env::var("HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED").as_deref() == Ok("1") {
        let mut command = Command::new(root.join("ci/rust-script-bin/rust-script"));
        command.arg("--force").arg(root.join(script));
        command
    } else {
        Command::new(root.join(script))
    }
}

/// Nextest's JSON list of the suite: the prepared runner's when the node requires prepared
/// executables, `cargo nextest`'s otherwise.
fn nextest_list(root: &Path, nextest_args: &[String]) -> Result<serde_json::Value, String> {
    let mut command = if env::var("HERMIT_PREPARED_NEXTEST_REQUIRED").as_deref() == Ok("1") {
        let mut command = rust_script(root, "ci/nextest-binaries.rs");
        command.arg("list");
        command
    } else {
        let mut command = Command::new("cargo");
        command.args(["nextest", "list"]);
        command
    };
    let output = command
        .args(nextest_args)
        .args(["--message-format", "json"])
        .current_dir(root)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("cannot start the Nextest list: {error}"))?;
    if !output.status.success() {
        return Err(format!("the Nextest list failed: {}", output.status));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("the Nextest list is not JSON: {error}"))
}

/// The Nextest filter expression that leaves the ledger's skipped rows out of the run, each named
/// exactly by its test function, or `None` when no row is skipped.
fn skip_filter(declaration: &Declaration, rows: &[Row]) -> Option<String> {
    let tests = rows
        .iter()
        .filter(|row| !row.outcome.is_run())
        .filter_map(|row| row.test(declaration.id_prefix))
        .map(|test| format!("test(={test})"))
        .collect::<Vec<_>>();
    (!tests.is_empty()).then(|| format!("not ({})", tests.join(" | ")))
}

/// `nextest_args` with `-E filter` added before any `--`, after which Nextest passes arguments to
/// the test binaries.
fn with_filter(nextest_args: &[String], filter: Option<&str>) -> Vec<String> {
    let mut args = nextest_args.to_vec();
    if let Some(filter) = filter {
        let at = args
            .iter()
            .position(|argument| argument == "--")
            .unwrap_or(args.len());
        args.splice(at..at, ["-E".to_string(), filter.to_string()]);
    }
    args
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The node: see the module documentation and [`usage`].
/// Where one counted run keeps what the node reads back.
struct RunPaths {
    /// The counted run's own schema-2 report: this script's input, not the node's result.
    all: PathBuf,
    cpu_report: PathBuf,
    events: PathBuf,
    proof: PathBuf,
}

/// The counted run of the whole population: the required tests and the gap rows together, so
/// the gap rows run under the same executables, budgets and backend. NEXTEST_RETRIES=0 overrides
/// any inherited value, the profile and every per-test override (measured with cargo-nextest
/// 0.9.100), so every test runs exactly once. RUST_BACKTRACE=0 keeps a panic's output in the form
/// the signatures match (std's "note: run with `RUST_BACKTRACE=1`" line rather than a backtrace),
/// whatever the caller's environment.
fn counted_run(
    root: &Path,
    declaration: &Declaration,
    nextest_args: &[String],
    population: usize,
    paths: &RunPaths,
) -> Command {
    let mut command = Command::new(root.join("ci/run-nextest-counted.sh"));
    command
        .args(nextest_args)
        .env("HERMIT_TEST_BACKEND", declaration.backend)
        .env(known_gaps::BACKEND_PROOF_ENV, &paths.proof)
        .env("NEXTEST_RETRIES", "0")
        .env("RUST_BACKTRACE", "0")
        .env("NEXTEST_EXPECTED_EXECUTED", population.to_string())
        .env("DAGRUN_TEST_COUNTS_PATH", &paths.all)
        .env("HERMIT_NEXTEST_CPU_REPORT_PATH", &paths.cpu_report)
        .env("HERMIT_NEXTEST_EVENTS_PATH", &paths.events)
        .current_dir(root);
    command
}

fn run(nextest_args: &[String]) -> Result<bool, String> {
    if let Some(argument) = nextest_args
        .iter()
        .find(|argument| *argument == "--retries" || argument.starts_with("--retries="))
    {
        return Err(format!(
            "{argument}: this node runs every test exactly once; it sets NEXTEST_RETRIES=0 itself"
        ));
    }
    if let Some(argument) = nextest_args.iter().find(|argument| {
        matches!(argument.as_str(), "-E" | "--filter-expr")
            || argument.starts_with("--filter-expr=")
    }) {
        return Err(format!(
            "{argument}: this node chooses its tests itself, the whole suite less the ledger's skipped rows, which it leaves out with its own filter expression"
        ));
    }
    let ledger = env::var(known_gaps::LEDGER_ENV).map_err(|_| {
        format!(
            "{} is not set: the node's ledger comes from its environment, which ci/dag/validate.json declares",
            known_gaps::LEDGER_ENV
        )
    })?;
    let declaration = known_gaps::declaration_for_ledger(&ledger).ok_or_else(|| {
        format!(
            "{}={ledger} is not a declared known-gap ledger (ci/manifest-plan/src/known_gaps.rs DECLARATIONS)",
            known_gaps::LEDGER_ENV
        )
    })?;
    let root = repository_root(&ledger)?;
    let (rows, tests) = read_declared(&root, declaration)?;
    if let Some(errors) = static_violations(declaration, &rows, &tests) {
        return Err(format!("{ledger} breaks its rules:\n{errors}"));
    }
    let skipped = rows
        .iter()
        .filter(|row| !row.outcome.is_run())
        .collect::<Vec<_>>();
    println!(
        "kvm-gap-ledger: {ledger}: {} gap row(s) for {} on --backend={}, {} of them skipped",
        rows.len(),
        declaration.node,
        declaration.backend,
        skipped.len()
    );
    for row in &skipped {
        println!(
            "KVM-GAP-SKIPPED {} {} {}: not run on --backend={} (approval {})",
            row.id,
            row.gap,
            row.issue,
            declaration.backend,
            match &row.approval {
                known_gaps::Approval::Baseline => known_gaps::BASELINE_APPROVAL,
                known_gaps::Approval::Url(url) => url.as_str(),
            }
        );
    }
    let skipped_tests = skipped
        .iter()
        .filter_map(|row| row.test(declaration.id_prefix))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let nextest_args = with_filter(nextest_args, skip_filter(declaration, &rows).as_deref());
    let expected_required = env::var("NEXTEST_EXPECTED_EXECUTED")
        .map_err(|_| {
            "NEXTEST_EXPECTED_EXECUTED (the number of required tests) is not set".to_string()
        })?
        .parse::<usize>()
        .map_err(|error| format!("NEXTEST_EXPECTED_EXECUTED: {error}"))?;
    let population = listed_population(
        &nextest_list(&root, &nextest_args)?,
        declaration,
        &tests,
        &skipped_tests,
    )?;
    plan(declaration, &rows, &population, expected_required)?;
    let scratch = env::temp_dir().join(format!(
        "kvm-gap-ledger-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    fs::create_dir(&scratch)
        .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
    let counts = env::var_os("DAGRUN_TEST_COUNTS_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    // The CPU report and the event stream are this node's evidence of each failure's cause and
    // output, so they stay beside its results (in the scratch directory without a results path).
    let evidence_base = counts.clone().unwrap_or_else(|| scratch.join("node.json"));
    let paths = RunPaths {
        all: scratch.join("all-tests.json"),
        cpu_report: with_suffix(&evidence_base, ".cpu.json"),
        events: with_suffix(&evidence_base, ".events.jsonl"),
        proof: with_suffix(&evidence_base, ".backend-proof"),
    };
    // A fresh, empty proof: only this run's verified runs can fill it.
    fs::write(&paths.proof, "")
        .map_err(|error| format!("cannot create {}: {error}", paths.proof.display()))?;
    let mut child = counted_run(&root, declaration, &nextest_args, population.len(), &paths)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("cannot start ci/run-nextest-counted.sh: {error}"))?;
    // Its libtest-style summary counts the gap rows as failures; prefix it, so that it reads as
    // the counted run's own count and not the node's.
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(|error| format!("cannot read the counted run: {error}"))?;
            println!("kvm-gap-ledger: all-tests run: {line}");
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for ci/run-nextest-counted.sh: {error}"))?;
    let code = status.code().unwrap_or(-1);
    let read = |path: &Path, what: &str| {
        fs::read(path).map_err(|error| {
            format!(
                "the counted run (exit {code}) left no {what} at {}: {error}",
                path.display()
            )
        })
    };
    let all = read(&paths.all, "results").and_then(|bytes| TestResults::from_json_slice(&bytes));
    let ends = read(&paths.cpu_report, "CPU report").and_then(|bytes| {
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| format!("the CPU report is not JSON: {error}"))
            .and_then(|report| attempt_ends(&report))
    });
    let outputs = read(&paths.events, "event stream")
        .and_then(|bytes| failure_outputs(&String::from_utf8_lossy(&bytes)));
    let proof_lines = fs::read_to_string(&paths.proof).ok().map(|text| {
        text.lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
    });
    println!(
        "kvm-gap-ledger: backend proof ({}): {}",
        paths.proof.display(),
        match &proof_lines {
            None => "no file".to_string(),
            Some(lines) if lines.is_empty() => "empty".to_string(),
            Some(lines) => lines.join(", "),
        }
    );
    if counts.is_some() {
        // The scratch directory is this process's own, created above, and holds nothing kept.
        let _ = fs::remove_dir_all(&scratch);
    } else {
        println!(
            "kvm-gap-ledger: DAGRUN_TEST_COUNTS_PATH is unset; the CPU report, event stream and backend proof are in {}",
            scratch.display()
        );
    }
    let (all, ends, outputs) = (all?, ends?, outputs?);
    let verdict = judge(
        declaration,
        &rows,
        &population,
        &Evidence {
            all: &all,
            status: code,
            ends: &ends,
            outputs: &outputs,
            proof: proof_lines.as_deref(),
        },
    )?;
    for line in &verdict.lines {
        println!("{line}");
    }
    let executed = verdict.results.executed_tests;
    let node_results = verdict.results.results.as_deref().unwrap_or_default();
    // The libtest-style summary of the node's own results: only required passes are passes.
    println!("running {executed} tests");
    println!(
        "test result: {}. {} passed; {} failed; 0 ignored; {} filtered out",
        if verdict.ok { "ok" } else { "FAILED" },
        node_results.iter().filter(|result| result.passed).count(),
        node_results
            .iter()
            .filter(|result| result.is_blocking_failure())
            .count(),
        verdict.results.filtered_tests
    );
    match &counts {
        Some(path) => verdict
            .results
            .write_diagnostic_typed(path)
            .map_err(|error| format!("cannot write the node's results: {error}"))?,
        None => println!("kvm-gap-ledger: DAGRUN_TEST_COUNTS_PATH is unset; no results written"),
    }
    Ok(verdict.ok)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let args = env::args().skip(1).collect::<Vec<_>>();
    let root = || env::current_dir().map_err(|error| format!("no working directory: {error}"));
    let outcome = match args.as_slice() {
        [command] if command == "check" => root().and_then(|root| check(&root, None)),
        [command, ledger] if command == "check" => {
            root().and_then(|root| check(&root, Some(ledger)))
        }
        [command] if command == "history" => root().and_then(|root| history(&root, None)),
        [command, ledger] if command == "history" => {
            root().and_then(|root| history(&root, Some(ledger)))
        }
        [command, separator, nextest_args @ ..] if command == "run" && separator == "--" => {
            run(nextest_args)
        }
        [help] if help == "-h" || help == "--help" => {
            println!("{}", usage());
            Ok(true)
        }
        _ => Err(format!("unrecognized arguments {args:?}\n{}", usage())),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("kvm-gap-ledger: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> PathBuf {
        let source = PathBuf::from(file!());
        let source = if source.is_absolute() {
            source
        } else {
            env::current_dir().unwrap().join(source)
        };
        source.parent().unwrap().parent().unwrap().to_path_buf()
    }

    /// A suite of five tests: `proof` proves the backend, `g` and `t2` are gap rows.
    const DECLARATION: Declaration = Declaration {
        node: "privileged-test.example_kvm",
        ledger: "suite.kvm-gaps.tsv",
        suite_source: "suite.rs",
        id_prefix: "pkg::suite$",
        backend: "kvm",
        backend_proof_test: "proof",
    };
    const ISSUE: &str = "https://github.com/rrnewton/hermit/issues/4929";
    const REFUSAL: &str = "unsupported_preemption: --backend=kvm refuses --max-timeslice";

    fn id(test: &str) -> String {
        format!("pkg::suite${test}")
    }

    fn ledger_text(extra: &[&str]) -> String {
        let mut text = format!(
            "# comment\n{}\n{}\tG\t{ISSUE}\tfailed\tsamples=20 progress=\tbaseline\n{}\tT2\t{ISSUE}\tcpu_timeout\t-\tbaseline\n",
            known_gaps::HEADER,
            id("g"),
            id("t2")
        );
        for row in extra {
            text.push_str(row);
            text.push('\n');
        }
        text
    }

    fn rows() -> Vec<Row> {
        known_gaps::parse(&ledger_text(&[])).unwrap()
    }

    /// A skipped row of the example suite, its test `test`.
    fn skipped_row(test: &str) -> Row {
        Row {
            id: id(test),
            gap: "C".into(),
            issue: ISSUE.into(),
            outcome: Outcome::Skipped,
            signature: None,
            approval: known_gaps::Approval::Baseline,
        }
    }

    fn population() -> BTreeSet<String> {
        ["a", "b", "g", "proof", "t2"]
            .iter()
            .map(|test| id(test))
            .collect()
    }

    fn report(outcomes: &[(&str, bool, u64)]) -> TestResults {
        let results = outcomes
            .iter()
            .map(|(test, passed, attempts)| TestResult::new(id(test), *passed, *attempts).unwrap())
            .collect::<Vec<_>>();
        TestResults::current(outcomes.len() as u64, 0, results).unwrap()
    }

    fn end(outcome: TestAttemptOutcome, detail: &str) -> AttemptEnd {
        AttemptEnd {
            outcome,
            detail: detail.into(),
        }
    }

    /// The expected run: a, b and proof pass, g fails with its signature, t2 hits its CPU budget.
    struct Run {
        all: TestResults,
        status: i32,
        ends: BTreeMap<String, AttemptEnd>,
        outputs: BTreeMap<String, String>,
        proof: Option<Vec<String>>,
    }

    fn expected_run() -> Run {
        Run {
            all: report(&[
                ("a", true, 1),
                ("b", true, 1),
                ("g", false, 1),
                ("proof", true, 1),
                ("t2", false, 1),
            ]),
            status: NEXTEST_TEST_FAILURES,
            ends: [
                (id("a"), end(TestAttemptOutcome::Passed, "")),
                (id("b"), end(TestAttemptOutcome::Passed, "")),
                (
                    id("g"),
                    end(TestAttemptOutcome::Failed, "exited with status 101"),
                ),
                (id("proof"), end(TestAttemptOutcome::Passed, "")),
                (
                    id("t2"),
                    end(
                        TestAttemptOutcome::CpuTimeout,
                        "was stopped by its CPU budget: 22.0 s of CPU used against a budget of 22.0 s",
                    ),
                ),
            ]
            .into_iter()
            .collect(),
            outputs: [(
                id("g"),
                "panicked at x:\npositive=3 samples=20 progress=19882\n".into(),
            )]
            .into_iter()
            .collect(),
            proof: Some(vec!["kvm".into(), "kvm".into()]),
        }
    }

    fn judged(run: &Run) -> Result<Verdict, String> {
        judge(
            &DECLARATION,
            &rows(),
            &population(),
            &Evidence {
                all: &run.all,
                status: run.status,
                ends: &run.ends,
                outputs: &run.outputs,
                proof: run.proof.as_deref(),
            },
        )
    }

    fn result<'a>(verdict: &'a Verdict, test: &str) -> &'a TestResult {
        verdict
            .results
            .results
            .as_ref()
            .unwrap()
            .iter()
            .find(|result| result.id == id(test))
            .unwrap()
    }

    /// Every declared ledger keeps its static rules against its suite's source.
    #[test]
    fn every_declared_ledger_keeps_its_static_rules() {
        let root = repository();
        for declaration in known_gaps::DECLARATIONS {
            let (rows, tests) = read_declared(&root, declaration).unwrap();
            assert_eq!(static_violations(declaration, &rows, &tests), None);
            let required = known_gaps::required(declaration, &rows, &tests).len() as u64;
            assert_eq!(
                Some(required),
                known_gaps::required_tests(declaration),
                "{}: the suite less the ledger is not the node's pinned count",
                declaration.ledger
            );
        }
    }

    /// The deadline_slice_cap ledger's rows name their gaps' issues. Its last six rows, the
    /// lettered gaps of https://github.com/rrnewton/hermit/issues/4929, are skipped on KVM since
    /// the owner's directive of 2026-11-01 (minibeads dh-j7ocf tracks the fix), each citing the
    /// issue of its own gap.
    #[test]
    fn the_deadline_slice_cap_rows_name_their_issues() {
        let declaration =
            known_gaps::declaration_for_node("privileged-test.deadline_slice_cap_kvm").unwrap();
        let (rows, _) = read_declared(&repository(), declaration).unwrap();
        let expected = [
            (
                "itimer_a_private_descriptor_table_consumer_re_arms",
                "D",
                4485,
            ),
            ("itimer_a_sigsuspend_loop_takes_every_expiry", "C", 5040),
            (
                "itimer_a_wait_mask_is_withdrawn_when_the_wait_returns",
                "F'",
                4995,
            ),
            (
                "itimer_an_ignored_expiry_stops_the_timer_until_a_dequeue",
                "E",
                4929,
            ),
            (
                "itimer_generation_discard_follows_the_wait_mask",
                "E+F'",
                4995,
            ),
            ("itimer_mixed_masks_keep_the_timer_running", "G", 4929),
        ];
        assert_eq!(rows.len(), expected.len());
        for (row, (test, gap, issue)) in rows.iter().zip(expected) {
            assert_eq!(row.test(declaration.id_prefix), Some(test));
            assert_eq!(row.gap, gap, "{}", row.id);
            assert_eq!(
                row.issue,
                format!("https://github.com/rrnewton/hermit/issues/{issue}"),
                "{}",
                row.id
            );
            assert_eq!(row.outcome, Outcome::Skipped, "{}", row.id);
        }
    }

    #[test]
    fn the_population_comes_from_the_declared_binary() {
        let list = serde_json::json!({"rust-suites": {
            "pkg::suite": {"binary-name": "suite", "testcases": {
                "a": {"ignored": false, "filter-match": {"status": "matches"}},
                "skipped": {"ignored": true, "filter-match": {"status": "matches"}},
                "unselected": {"ignored": false, "filter-match": {"status": "mismatch"}}
            }},
            "pkg::other": {"binary-name": "other", "testcases": {
                "b": {"ignored": false, "filter-match": {"status": "matches"}}
            }}
        }});
        // The source declares a, the ignored skipped, and unselected; Nextest's filter leaves
        // unselected out, which the node refuses: it runs the whole suite.
        let declared = |names: &[(&str, bool)]| {
            names
                .iter()
                .map(|(name, ignored)| (name.to_string(), *ignored))
                .collect::<BTreeMap<_, _>>()
        };
        let none = BTreeSet::new();
        let error = listed_population(
            &list,
            &DECLARATION,
            &declared(&[("a", false), ("skipped", true), ("unselected", false)]),
            &none,
        )
        .unwrap_err();
        assert!(
            error.contains("Not ignored but filtered out: unselected"),
            "{error}"
        );
        let whole = serde_json::json!({"rust-suites": {"pkg::suite": {"testcases": {
            "a": {"ignored": false, "filter-match": {"status": "matches"}},
            "skipped": {"ignored": true, "filter-match": {"status": "matches"}}
        }}}});
        assert_eq!(
            listed_population(
                &whole,
                &DECLARATION,
                &declared(&[("a", false), ("skipped", true)]),
                &none,
            )
            .unwrap(),
            [id("a")].into_iter().collect()
        );
        let error = listed_population(
            &serde_json::json!({"rust-suites": {}}),
            &DECLARATION,
            &declared(&[("a", false)]),
            &none,
        )
        .unwrap_err();
        assert!(error.contains("no test binary pkg::suite"), "{error}");
    }

    /// A skipped row must be left out of the run, and nothing else may be: the population is the
    /// suite less its ignored tests and exactly the skipped rows.
    #[test]
    fn the_population_leaves_out_exactly_the_skipped_rows() {
        let declared = [("a", false), ("proof", false), ("s", false)]
            .iter()
            .map(|(name, ignored)| (name.to_string(), *ignored))
            .collect::<BTreeMap<_, _>>();
        let skipped = ["s".to_string()].into_iter().collect::<BTreeSet<_>>();
        let list = |s: &str, a: &str| {
            serde_json::json!({"rust-suites": {"pkg::suite": {"testcases": {
                "a": {"ignored": false, "filter-match": {"status": a}},
                "proof": {"ignored": false, "filter-match": {"status": "matches"}},
                "s": {"ignored": false, "filter-match": {"status": s}}
            }}}})
        };
        assert_eq!(
            listed_population(
                &list("mismatch", "matches"),
                &DECLARATION,
                &declared,
                &skipped
            )
            .unwrap(),
            [id("a"), id("proof")].into_iter().collect()
        );
        // The filter did not leave the skipped row out.
        let error = listed_population(
            &list("matches", "matches"),
            &DECLARATION,
            &declared,
            &skipped,
        )
        .unwrap_err();
        assert!(error.contains("Skipped rows selected anyway: s"), "{error}");
        // It left out a test that is not a skipped row.
        let error = listed_population(
            &list("mismatch", "mismatch"),
            &DECLARATION,
            &declared,
            &skipped,
        )
        .unwrap_err();
        assert!(
            error.contains("Not ignored but filtered out: a."),
            "{error}"
        );
        // Without the row, leaving s out is a narrowed run.
        let error = listed_population(
            &list("mismatch", "matches"),
            &DECLARATION,
            &declared,
            &BTreeSet::new(),
        )
        .unwrap_err();
        assert!(
            error.contains("Not ignored but filtered out: s."),
            "{error}"
        );
    }

    /// The filter names each skipped row exactly, and goes before the test-binary arguments.
    #[test]
    fn skipped_rows_are_left_out_by_an_exact_filter() {
        let mut rows = rows();
        assert_eq!(skip_filter(&DECLARATION, &rows), None);
        let args = ["-p".to_string(), "pkg".to_string()];
        assert_eq!(with_filter(&args, None), args);
        rows.extend([skipped_row("s1"), skipped_row("s2")]);
        let filter = skip_filter(&DECLARATION, &rows).unwrap();
        assert_eq!(filter, "not (test(=s1) | test(=s2))");
        assert_eq!(
            with_filter(&args, Some(&filter)),
            ["-p", "pkg", "-E", filter.as_str()]
        );
        let separated = [
            "-p".to_string(),
            "--".to_string(),
            "--nocapture".to_string(),
        ];
        assert_eq!(
            with_filter(&separated, Some(&filter)),
            ["-p", "-E", filter.as_str(), "--", "--nocapture"]
        );
    }

    /// The plan refuses a skipped row in the run, and does not count one as required.
    #[test]
    fn the_plan_keeps_skipped_rows_out_of_the_run_and_the_count() {
        let mut rows = rows();
        rows.push(skipped_row("s"));
        // a, b and proof are required; g and t2 are run as gaps; s is not run.
        assert_eq!(
            plan(&DECLARATION, &rows, &population(), 3).unwrap().len(),
            3
        );
        let error = plan(&DECLARATION, &rows, &population(), 4).unwrap_err();
        assert!(error.contains("1 skipped rows are not run"), "{error}");
        let mut with_s = population();
        with_s.insert(id("s"));
        let error = plan(&DECLARATION, &rows, &with_s, 3).unwrap_err();
        assert!(
            error.contains("skipped rows name tests the run would execute: pkg::suite$s"),
            "{error}"
        );
    }

    /// A run of a ledger with a skipped row: the row has no result, the node passes, and its
    /// summary counts the row as not run.
    #[test]
    fn a_skipped_row_has_no_result_and_is_counted_as_not_run() {
        let mut rows = rows();
        rows.push(skipped_row("s"));
        let run = expected_run();
        let verdict = judge(
            &DECLARATION,
            &rows,
            &population(),
            &Evidence {
                all: &run.all,
                status: run.status,
                ends: &run.ends,
                outputs: &run.outputs,
                proof: run.proof.as_deref(),
            },
        )
        .unwrap();
        assert!(verdict.ok, "{:?}", verdict.lines);
        let results = verdict.results.results.as_ref().unwrap();
        assert_eq!(results.len(), 5);
        assert!(results.iter().all(|result| result.id != id("s")));
        assert!(
            verdict
                .lines
                .last()
                .unwrap()
                .contains("1 skipped row(s) not run (KVM-GAP-SKIPPED); PASS"),
            "{:?}",
            verdict.lines
        );
    }

    /// The review's balanced substitution as Nextest would list it: the required test ignored and
    /// a replacement the source parser does not declare. Refused, test by test, although the
    /// number of tests run is unchanged.
    #[test]
    fn a_substituted_test_is_refused_by_identity() {
        let declared = [("a".to_string(), false), ("proof".to_string(), false)]
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let substituted = serde_json::json!({"rust-suites": {"pkg::suite": {"testcases": {
            "a": {"ignored": false, "filter-match": {"status": "matches"}},
            "proof": {"ignored": true, "filter-match": {"status": "matches"}},
            "replacement": {"ignored": false, "filter-match": {"status": "matches"}}
        }}}});
        let error =
            listed_population(&substituted, &DECLARATION, &declared, &BTreeSet::new()).unwrap_err();
        assert!(
            error.contains("Listed but not declared: replacement"),
            "{error}"
        );
        assert!(error.contains("Ignored on one side only: proof"), "{error}");
        // Declared but missing from the list (a test cfg'd out of the build) is refused too.
        let missing = serde_json::json!({"rust-suites": {"pkg::suite": {"testcases": {
            "a": {"ignored": false, "filter-match": {"status": "matches"}}
        }}}});
        let error =
            listed_population(&missing, &DECLARATION, &declared, &BTreeSet::new()).unwrap_err();
        assert!(error.contains("Declared but not listed: proof"), "{error}");
    }

    #[test]
    fn the_plan_holds_the_required_count_to_its_pin() {
        assert_eq!(
            plan(&DECLARATION, &rows(), &population(), 3).unwrap().len(),
            3
        );
        let error = plan(&DECLARATION, &rows(), &population(), 4).unwrap_err();
        assert!(error.contains("pinned to 4"), "{error}");
        let mut smaller = population();
        smaller.remove(&id("g"));
        let error = plan(&DECLARATION, &rows(), &smaller, 3).unwrap_err();
        assert!(error.contains("would not execute"), "{error}");
        // The backend-proof test must be in the selection.
        let mut without_proof = population();
        without_proof.remove(&id("proof"));
        without_proof.insert(id("c"));
        let error = plan(&DECLARATION, &rows(), &without_proof, 3).unwrap_err();
        assert!(
            error.contains("backend-proof test pkg::suite$proof is not in the run's selection"),
            "{error}"
        );
    }

    /// A run whose results lack the backend-proof test is refused, not judged.
    #[test]
    fn a_run_without_the_proof_result_is_refused() {
        let mut run = expected_run();
        run.all = report(&[
            ("a", true, 1),
            ("b", true, 1),
            ("g", false, 1),
            ("t2", false, 1),
        ]);
        run.ends.remove(&id("proof"));
        let error = judged(&run).unwrap_err();
        assert!(
            error.contains("no result for the backend-proof test pkg::suite$proof"),
            "{error}"
        );
    }

    #[test]
    fn the_expected_run_passes_with_its_gaps_as_diagnostics() {
        let verdict = judged(&expected_run()).unwrap();
        assert!(verdict.ok, "{:?}", verdict.lines);
        let g = result(&verdict, "g");
        assert!(g.is_diagnostic_failure() && !g.passed);
        assert_eq!(
            g.diagnostic.as_deref(),
            Some(format!("KNOWN-KVM-GAP G {ISSUE}").as_str())
        );
        let t2 = result(&verdict, "t2");
        assert!(t2.is_diagnostic_failure());
        // The CPU-budget stop is labelled as one, not as an ordinary failure.
        assert_eq!(
            t2.attempt_results.as_ref().unwrap()[0].outcome,
            TestAttemptOutcome::CpuTimeout
        );
        assert!(result(&verdict, "proof").passed);
        let passed = verdict
            .results
            .results
            .as_ref()
            .unwrap()
            .iter()
            .filter(|r| r.passed)
            .count();
        assert_eq!(passed, 3, "a gap row is never counted as a pass");
        assert!(
            verdict
                .lines
                .iter()
                .any(|line| line.starts_with("KNOWN-KVM-GAP pkg::suite$g G"))
        );
    }

    #[test]
    fn a_failing_required_test_fails_the_node_with_its_cause() {
        let mut run = expected_run();
        run.all = report(&[
            ("a", false, 1),
            ("b", true, 1),
            ("g", false, 1),
            ("proof", true, 1),
            ("t2", false, 1),
        ]);
        run.ends.insert(
            id("a"),
            end(
                TestAttemptOutcome::WallTimeout,
                "was stopped by signal 15 from Nextest",
            ),
        );
        let verdict = judged(&run).unwrap();
        assert!(!verdict.ok);
        let a = result(&verdict, "a");
        assert!(a.is_blocking_failure());
        assert_eq!(
            a.attempt_results.as_ref().unwrap()[0].outcome,
            TestAttemptOutcome::WallTimeout
        );
        assert!(
            verdict
                .lines
                .iter()
                .any(|line| line.starts_with("KVM-REQUIRED-FAIL pkg::suite$a"))
        );
    }

    #[test]
    fn a_passing_gap_row_fails_the_node_with_the_promotion_message() {
        let mut run = expected_run();
        run.all = report(&[
            ("a", true, 1),
            ("b", true, 1),
            ("g", true, 1),
            ("proof", true, 1),
            ("t2", false, 1),
        ]);
        run.ends
            .insert(id("g"), end(TestAttemptOutcome::Passed, ""));
        let verdict = judged(&run).unwrap();
        assert!(!verdict.ok);
        let g = result(&verdict, "g");
        assert!(g.is_blocking_failure() && !g.passed);
        assert!(
            verdict
                .lines
                .iter()
                .any(|line| line.contains("now passing, promote to required"))
        );
    }

    /// A gap row that fails for a different reason than its gap is not the known gap.
    #[test]
    fn a_gap_row_failing_another_way_fails_the_node() {
        let mut run = expected_run();
        run.outputs
            .insert(id("g"), "thread panicked: a crash\n".into());
        let verdict = judged(&run).unwrap();
        assert!(!verdict.ok);
        assert!(result(&verdict, "g").is_blocking_failure());
        assert!(verdict.lines.iter().any(|line| {
            line.starts_with("KVM-GAP-WRONG-FAILURE pkg::suite$g")
                && line.contains("lacks the signature")
        }));
        // The CPU-budget row hit by an ordinary failure instead.
        let mut run = expected_run();
        run.ends.insert(
            id("t2"),
            end(TestAttemptOutcome::Failed, "exited with status 2"),
        );
        let verdict = judged(&run).unwrap();
        assert!(!verdict.ok);
        assert!(result(&verdict, "t2").is_blocking_failure());
        // The signature row stopped by a wall-clock backstop instead.
        let mut run = expected_run();
        run.ends.insert(
            id("g"),
            end(
                TestAttemptOutcome::WallTimeout,
                "was stopped by signal 15 from Nextest",
            ),
        );
        let verdict = judged(&run).unwrap();
        assert!(!verdict.ok);
        assert!(result(&verdict, "g").is_blocking_failure());
    }

    /// The node runs every test once; a retried result is refused, whichever way it ended.
    #[test]
    fn a_retried_result_is_refused() {
        let mut run = expected_run();
        run.all = report(&[
            ("a", true, 2),
            ("b", true, 1),
            ("g", false, 1),
            ("proof", true, 1),
            ("t2", false, 1),
        ]);
        let error = judged(&run).unwrap_err();
        assert!(
            error.contains("exactly once (NEXTEST_RETRIES=0)"),
            "{error}"
        );
    }

    /// The counted run gets zero retries and the declared backend, and the node refuses an
    /// argument that would ask for retries, or a filter expression beside its own.
    #[test]
    fn the_counted_run_allows_no_retry() {
        for arguments in [["--retries", "2"].as_slice(), ["--retries=2"].as_slice()] {
            let arguments = arguments
                .iter()
                .map(|argument| argument.to_string())
                .collect::<Vec<_>>();
            let error = run(&arguments).unwrap_err();
            assert!(error.contains("exactly once"), "{error}");
        }
        for arguments in [
            ["-E", "all()"].as_slice(),
            ["--filter-expr", "all()"].as_slice(),
            ["--filter-expr=all()"].as_slice(),
        ] {
            let arguments = arguments
                .iter()
                .map(|argument| argument.to_string())
                .collect::<Vec<_>>();
            let error = run(&arguments).unwrap_err();
            assert!(error.contains("chooses its tests itself"), "{error}");
        }
        let paths = RunPaths {
            all: "all.json".into(),
            cpu_report: "node.json.cpu.json".into(),
            events: "node.json.events.jsonl".into(),
            proof: "proof".into(),
        };
        let command = counted_run(
            Path::new("/repo"),
            &DECLARATION,
            &["-p".into(), "pkg".into()],
            5,
            &paths,
        );
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_str().unwrap().to_string(),
                    value.map(|value| value.to_str().unwrap().to_string()),
                )
            })
            .collect::<BTreeMap<_, _>>();
        for (name, value) in [
            ("NEXTEST_RETRIES", "0"),
            ("RUST_BACKTRACE", "0"),
            ("HERMIT_TEST_BACKEND", "kvm"),
            ("NEXTEST_EXPECTED_EXECUTED", "5"),
            (known_gaps::BACKEND_PROOF_ENV, "proof"),
            ("DAGRUN_TEST_COUNTS_PATH", "all.json"),
            ("HERMIT_NEXTEST_CPU_REPORT_PATH", "node.json.cpu.json"),
            ("HERMIT_NEXTEST_EVENTS_PATH", "node.json.events.jsonl"),
        ] {
            assert_eq!(
                environment.get(name),
                Some(&Some(value.to_string())),
                "{name}"
            );
        }
    }

    /// The backend-proof test passes only with a proof of the declared backend.
    #[test]
    fn an_unproven_backend_fails_the_node() {
        for proof in [
            None,
            Some(vec![]),
            Some(vec!["ptrace".to_string()]),
            Some(vec!["kvm".into(), "ptrace".into()]),
        ] {
            let mut run = expected_run();
            run.proof = proof.clone();
            let verdict = judged(&run).unwrap();
            assert!(!verdict.ok, "{proof:?}");
            assert!(result(&verdict, "proof").is_blocking_failure(), "{proof:?}");
            assert!(
                verdict
                    .lines
                    .iter()
                    .any(|line| line.starts_with("KVM-BACKEND-UNPROVEN")),
                "{proof:?}"
            );
        }
    }

    #[test]
    fn an_incomplete_or_contradicted_run_is_refused() {
        let mut run = expected_run();
        run.all = report(&[
            ("a", true, 1),
            ("b", true, 1),
            ("g", false, 1),
            ("proof", true, 1),
        ]);
        assert!(judged(&run).unwrap_err().contains("missing"));
        let mut run = expected_run();
        run.status = 0;
        assert!(judged(&run).unwrap_err().contains("exited 0"));
        let mut run = expected_run();
        run.ends.remove(&id("b"));
        assert!(
            judged(&run)
                .unwrap_err()
                .contains("no attempt of pkg::suite$b")
        );
        let mut run = expected_run();
        run.ends.insert(
            id("b"),
            end(TestAttemptOutcome::Failed, "exited with status 1"),
        );
        assert!(judged(&run).unwrap_err().contains("the CPU report says"));
    }

    #[test]
    fn attempt_ends_label_each_completion() {
        let report = serde_json::json!({"schema": 3, "attempts": [
            {"identity": {"package": "pkg", "binary": "pkg::suite", "test": "a", "attempt": 1}, "completion": {"kind": "exit", "code": 0}},
            {"identity": {"package": "pkg", "binary": "pkg::suite", "test": "b", "attempt": 1}, "completion": {"kind": "exit", "code": 101}},
            {"identity": {"package": "pkg", "binary": "pkg::suite", "test": "c", "attempt": 1}, "completion": {"kind": "cpu_timeout", "cpu_budget_usec": 22000000, "observed_cpu_usec": 22100000}},
            {"identity": {"package": "pkg", "binary": "pkg::suite", "test": "d", "attempt": 1}, "completion": {"kind": "supervisor_signal", "signal": 15}},
            {"identity": {"package": "pkg", "binary": "pkg::suite", "test": "e", "attempt": 1}, "completion": {"kind": "signal", "signal": 9}}
        ]});
        let ends = attempt_ends(&report).unwrap();
        assert_eq!(ends[&id("a")].outcome, TestAttemptOutcome::Passed);
        assert_eq!(
            ends[&id("b")],
            end(TestAttemptOutcome::Failed, "exited with status 101")
        );
        assert_eq!(ends[&id("c")].outcome, TestAttemptOutcome::CpuTimeout);
        assert!(
            ends[&id("c")]
                .detail
                .contains("22.1 s of CPU used against a budget of 22.0 s")
        );
        assert_eq!(ends[&id("d")].outcome, TestAttemptOutcome::WallTimeout);
        assert_eq!(ends[&id("e")].outcome, TestAttemptOutcome::Failed);
    }

    #[test]
    fn failure_outputs_come_from_failed_events() {
        let events = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"suite","event":"started","test_count":2}"#,
            r#"{"type":"test","event":"ok","name":"pkg::suite$a","exec_time":0.1}"#,
            serde_json::json!({"type": "test", "event": "failed", "name": id("g"), "stdout": format!("x\n{REFUSAL}\n")})
        );
        let outputs = failure_outputs(&events).unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(outputs[&id("g")].contains(REFUSAL));
        assert!(failure_outputs("not json\n").is_err());
    }

    // -----------------------------------------------------------------------------------------
    // The history gate against real git history.

    /// A scratch repository with the test suite, committed by explicit identity, with no hooks
    /// and no inherited configuration.
    struct Repository {
        root: PathBuf,
    }

    impl Repository {
        fn new(name: &str) -> Self {
            let root = env::temp_dir().join(format!(
                "kvm-gap-ledger-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            let repository = Self { root };
            repository.git(&["init", "--quiet", "--initial-branch=main"]);
            repository.write("README", "fixture\n");
            repository.commit("before the ledger");
            repository
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args([
                    "-c",
                    "user.name=fixture",
                    "-c",
                    "user.email=fixture@example.com",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        }

        fn write(&self, path: &str, text: &str) {
            fs::write(self.root.join(path), text).unwrap();
        }

        fn suite(&self, tests: &[&str]) {
            let mut source = String::new();
            for test in tests {
                source.push_str(&format!("#[test]\nfn {test}() {{}}\n\n"));
            }
            self.write(DECLARATION.suite_source, &source);
        }

        fn commit(&self, message: &str) {
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "--allow-empty", "-m", message]);
        }

        fn judge(&self) -> Result<History, String> {
            history_of(&self.root, &DECLARATION)
        }
    }

    impl Drop for Repository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn introduced(name: &str) -> Repository {
        let repository = Repository::new(name);
        repository.suite(&["a", "b", "g", "proof", "t2"]);
        repository.write(DECLARATION.ledger, &ledger_text(&[]));
        repository.commit("introduce the ledger");
        repository
    }

    #[test]
    fn history_admits_the_introduction_and_a_promotion() {
        let repository = introduced("promotion");
        let history = repository.judge().unwrap();
        assert!(
            history.verdict.violations.is_empty(),
            "{:?}",
            history.verdict.violations
        );
        assert_eq!(
            (history.verdict.baseline_rows, history.verdict.required),
            (2, 3)
        );
        // Promote g.
        let text = ledger_text(&[]);
        let promoted = text
            .lines()
            .filter(|line| !line.starts_with("pkg::suite$g"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        repository.write(DECLARATION.ledger, &promoted);
        repository.commit("promote g");
        let history = repository.judge().unwrap();
        assert!(
            history.verdict.violations.is_empty(),
            "{:?}",
            history.verdict.violations
        );
        assert_eq!((history.versions, history.verdict.required), (2, 4));
    }

    /// The review's swap: drop the G row and add a required test as "baseline", the count
    /// unchanged. Refused.
    #[test]
    fn history_refuses_the_swap() {
        let repository = introduced("swap");
        let text = ledger_text(&[&format!(
            "{}\tT2\t{ISSUE}\tfailed\t{REFUSAL}\tbaseline",
            id("a")
        )]);
        let mut swapped = text
            .lines()
            .filter(|line| !line.starts_with("pkg::suite$g"))
            .collect::<Vec<_>>();
        let row = swapped.pop().unwrap();
        swapped.insert(2, row);
        repository.write(DECLARATION.ledger, &(swapped.join("\n") + "\n"));
        repository.commit("swap a row");
        let history = repository.judge().unwrap();
        assert_eq!(
            history.verdict.violations.len(),
            1,
            "{:?}",
            history.verdict.violations
        );
        assert!(
            history.verdict.violations[0]
                .starts_with("pkg::suite$a: its approval field says \"baseline\"")
        );
    }

    /// The review's made-up approval, alone and then bound by a commit message: unbound it is
    /// refused; bound, it is admitted, because no offline check can tell that the comment does
    /// not exist. That part stays with review.
    #[test]
    fn history_refuses_an_unbound_approval_and_admits_a_bound_one() {
        let fake = "https://github.com/rrnewton/hermit/issues/4954#issuecomment-1";
        let repository = introduced("approval");
        let added = format!("{}\tT2\t{ISSUE}\tfailed\t{REFUSAL}\t{fake}", id("b"));
        let mut lines = ledger_text(&[])
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        lines.insert(2, added);
        repository.write(DECLARATION.ledger, &(lines.join("\n") + "\n"));
        repository.commit("add b with a made-up approval");
        let history = repository.judge().unwrap();
        assert_eq!(
            history.verdict.violations.len(),
            1,
            "{:?}",
            history.verdict.violations
        );
        assert!(history.verdict.violations[0].contains("is bound to it by no commit message"));
        repository.commit(&format!(
            "bind it\n\n{} {fake} {}\n",
            known_gaps::GAP_APPROVAL_KEY,
            id("b")
        ));
        let history = repository.judge().unwrap();
        assert!(
            history.verdict.violations.is_empty(),
            "{:?}",
            history.verdict.violations
        );
        assert_eq!(history.verdict.approved_rows, 1);
    }

    #[test]
    fn history_refuses_a_deleted_required_test_without_approval() {
        let repository = introduced("removal");
        repository.suite(&["a", "g", "proof", "t2"]);
        repository.commit("delete b");
        let history = repository.judge().unwrap();
        assert_eq!(
            history.verdict.violations.len(),
            1,
            "{:?}",
            history.verdict.violations
        );
        assert!(history.verdict.violations[0].starts_with("pkg::suite$b: a required test left"));
    }

    #[test]
    fn history_refuses_to_judge_an_uncommitted_or_absent_ledger() {
        let repository = introduced("dirty");
        repository.write(
            DECLARATION.ledger,
            &ledger_text(&[]).replace("samples=20", "samples=21"),
        );
        assert!(
            repository
                .judge()
                .unwrap_err()
                .contains("differs from HEAD's")
        );
        let empty = Repository::new("absent");
        empty.suite(&["a", "proof"]);
        empty.commit("suite only");
        let error = empty.judge().unwrap_err();
        assert!(error.contains("is in no commit"), "{error}");
    }
}
