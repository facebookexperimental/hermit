//! `test-harness run` import mode: re-publish cell rows another runner wrote.
//!
//! With `E2E_IMPORT_RESULTS=<root>` the harness executes no cell. It reads the
//! rows that a Buck run of the same plan left under
//! `<root>/<lane>/manifest_<category>/results.jsonl` (the layout
//! `ci/buck-e2e/ingest.py` writes) and emits them through the normal
//! publication path, so `results.jsonl`, JUnit, `summary.json`, the retry
//! history and the dagrun test counts are produced exactly as for an executed
//! bucket. Every selected cell must be accounted for: a cell with no row and no
//! host inapplicability this machine confirms becomes an ERROR row. That is the
//! executed-equals-plan gate for an imported run, and nothing here may relax it.
//!
//! An imported cell may not end better than the same rows would have ended in
//! an executed run:
//!
//! - Every row must describe this commit's clean source, this checkout's test
//!   source, the current timeout policy, the `/proc` level the cell's test
//!   declares, and (when the binary was stamped) a binary built from this
//!   commit. Otherwise the cell is one `import-stale` ERROR.
//! - The cell's rows, in file order, must be a history a producer writes:
//!   attempts 1, 2, ... within the shared attempt cap, nothing after a PASS,
//!   and no HOST-INAPPLICABLE row (a producer reports host inapplicability
//!   only in `summary.json`). Otherwise the cell is one `import-history`
//!   ERROR. This is checked on the whole history, before anything is dropped.
//! - The producer retries every failure; this run's retry policy decides which
//!   of those retries it would have made. History after a failed attempt (a
//!   FAIL or an ERROR) that does not earn a retry here is dropped, so that
//!   attempt is the cell's verdict.
//! - The policy is the decision an executed run applies to its own attempt
//!   (`attempt_retry_cause` in `test-harness`), applied to the imported row.
//!   For an ERROR that is the skid-overshoot retry
//!   (`skid_overshoot_only_reports`,
//!   <https://github.com/rrnewton/hermit/issues/1845>) or the host-input
//!   retry (`host_input_change_only`). Neither trusts the row's labels: each
//!   re-decides the attempt from the report bytes the row retains, which must
//!   match the row's `verification_report_sha256`, and the row must record
//!   the exit, stdout and relaxation declarations of this checkout's cell. So
//!   an imported skid-overshoot ERROR whose retained evidence decides it
//!   continues to its producer's next attempt, as an executed one would, and
//!   any other ERROR (an untyped one, a report that does not match its digest,
//!   another declaration) ends the history.
//! - A PASS counts only if the producer recorded complete evidence for the
//!   execution that passed: that row's run id in `evidence_complete_executions`
//!   in the bucket's `summary.json`. Evidence from another execution of the
//!   same cell does not count.
//! - A producer's host-inapplicable claim counts only if this machine lacks a
//!   capability the cell requires, and the row carries this machine's reason.
//!
//! An imported row is rebound to this run before publication: its `run_id`
//! (and the run id its CPU observations are bound to) becomes this run's, and
//! the observations' outer attempt follows the row's attempt, which the ingest
//! assigned from Tpx's execution order. Its `source_tree_dirty` is set when this
//! checkout is dirty, as an executed row's would be. The rows' original run ids
//! are kept and reported.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::runner::CELL_RESULT_SCHEMA;
use crate::runner::CellResult;
use crate::runner::RunContext;
use crate::runner::SelectedCell;
use crate::runner::cell_timeouts;
use crate::runner::host_inapplicable_result;
use crate::runner::infrastructure_error_result;
use crate::runner::outcome_after_retries;
use crate::runner::proc_level_mismatch;
use crate::runner::test_digest;

pub const IMPORT_RESULTS_ENV: &str = "E2E_IMPORT_RESULTS";

/// The bucket directory of `(lane, category)` below an import root.
pub fn bucket_dir(root: &Path, lane: &str, category: &str) -> PathBuf {
    root.join(lane)
        .join(format!("manifest_{}", category.replace('-', "_")))
}

/// The decisions an executed run makes for itself, applied to imported rows.
pub struct ImportPolicy<'a> {
    /// Whether this run would retry after `row`, by the rule an executed run
    /// applies to its own attempt: its retry setting, the cell's
    /// `no_retry_reason`, and for an ERROR the skid-overshoot and host-input
    /// rules, which re-decide the attempt from its retained report bytes. It
    /// is consulted for every FAIL and ERROR row; a PASS ends the history.
    pub earns_retry: &'a dyn Fn(&SelectedCell, &CellResult) -> bool,
    /// Why this machine cannot run the cell, if it cannot.
    pub host_inapplicable: &'a dyn Fn(&SelectedCell) -> Option<String>,
}

/// What the import root says about one selected cell, already turned into the
/// rows the harness emits: every attempt in order, or one terminal row.
#[derive(Clone, Debug)]
pub struct ImportedCell {
    pub rows: Vec<CellResult>,
}

/// The import of one `run`: one entry per selected cell, in selection order.
pub struct ImportedRun {
    pub cells: Vec<ImportedCell>,
    /// The run ids the imported rows carried before they were rebound.
    pub source_run_ids: BTreeSet<String>,
    /// Selected cells the root had nothing for.
    pub missing: usize,
    /// Producer attempts dropped because the attempt before them would not
    /// have been retried by this run.
    pub dropped_retries: usize,
}

#[derive(Deserialize)]
struct Summary {
    #[serde(default)]
    host_inapplicable_cells: Vec<SummaryCell>,
    #[serde(default)]
    evidence_complete_executions: Vec<SummaryExecution>,
}

#[derive(Deserialize)]
struct SummaryCell {
    test: String,
    mode: String,
    backend: Option<String>,
    reason: Option<String>,
}

/// One producer execution of a cell, named by the run id its rows carry.
#[derive(Deserialize)]
struct SummaryExecution {
    test: String,
    mode: String,
    backend: Option<String>,
    run_id: String,
}

type Key = (String, String, String, String, Option<String>);

fn key(lane: &str, category: &str, test: &str, mode: &str, backend: Option<&str>) -> Key {
    (
        lane.into(),
        category.into(),
        test.into(),
        mode.into(),
        backend.map(str::to_owned),
    )
}

fn cell_key(cell: &SelectedCell) -> Key {
    key(
        &cell.test.lane,
        &cell.category,
        &cell.id.test,
        &cell.id.mode,
        cell.id.backend.as_deref(),
    )
}

fn error_row(context: &RunContext, cell: &SelectedCell, kind: &str, reason: String) -> CellResult {
    let mut row = infrastructure_error_result(context, cell, reason);
    row.error_kind = Some(kind.into());
    row
}

/// Why `row` cannot stand for `cell` in this run, if it cannot.
fn stale_reason(context: &RunContext, cell: &SelectedCell, row: &CellResult) -> Option<String> {
    if row.schema != CELL_RESULT_SCHEMA {
        return Some(format!(
            "imported row has schema {}, expected {CELL_RESULT_SCHEMA}",
            row.schema
        ));
    }
    if row.hermit_sha != context.source_sha {
        return Some(format!(
            "imported row was built from {}, this run is {}",
            row.hermit_sha, context.source_sha
        ));
    }
    if row.source_tree_dirty {
        return Some("imported row was produced from a dirty source tree".into());
    }
    if let Some(binary) = row.binary_build_sha.as_deref() {
        if binary != "unknown" && !(binary.len() >= 7 && context.source_sha.starts_with(binary)) {
            return Some(format!(
                "imported row ran a Hermit stamped {binary}, this run is {}",
                context.source_sha
            ));
        }
    }
    match test_digest(&context.root, &cell.test) {
        Ok(digest) if digest == row.test_sha256 => {}
        Ok(digest) => {
            return Some(format!(
                "imported row ran test source {}, this checkout has {digest}",
                row.test_sha256
            ));
        }
        Err(error) => {
            return Some(format!(
                "cannot digest this checkout's test source: {error}"
            ));
        }
    }
    if let Some(mismatch) = proc_level_mismatch(row, cell) {
        return Some(format!(
            "imported row is not at this cell's /proc level: {mismatch}"
        ));
    }
    match cell_timeouts(context, cell) {
        Ok(policy)
            if row.execution_cpu_timeout_seconds == Some(policy.cpu_seconds)
                && row.execution_wall_timeout_seconds == Some(policy.wall_seconds) => {}
        Ok(policy) => {
            return Some(format!(
                "imported row ran with cpu/wall timeouts {:?}/{:?}, this run's policy is {}/{}",
                row.execution_cpu_timeout_seconds,
                row.execution_wall_timeout_seconds,
                policy.cpu_seconds,
                policy.wall_seconds
            ));
        }
        Err(error) => return Some(format!("cannot resolve this cell's timeouts: {error}")),
    }
    // Each producer execution is its own --no-retry harness run, so its CPU
    // observations are bound to that run's id and outer attempt 1; the
    // ingest's attempt number is assigned afterwards.
    if row.cpu_observations.is_none() {
        return Some("imported row has no CPU observations".into());
    }
    let mut as_executed = row.clone();
    as_executed.attempt = 1;
    if let Err(error) = as_executed.require_cpu_observations() {
        return Some(format!(
            "imported row's CPU observations do not belong to its own execution: {error}"
        ));
    }
    None
}

/// Why `rows`, in file order, are not a history a producer writes, if they
/// are not. The executed path's own rule decides (`outcome_after_retries`),
/// and a producer never writes a HOST-INAPPLICABLE row.
fn history_error(rows: &[CellResult]) -> Option<String> {
    if let Some(row) = rows.iter().find(|row| row.outcome == "HOST-INAPPLICABLE") {
        return Some(format!(
            "imported attempt {} is a HOST-INAPPLICABLE row; a producer reports host inapplicability only in summary.json's host_inapplicable_cells",
            row.attempt
        ));
    }
    outcome_after_retries(rows.iter().map(|row| (row.attempt, row.outcome.as_str())))
        .err()
        .map(|error| format!("imported rows are not one producer history: {error}"))
}

/// Read the import root for `cells`. `results_path` is where this run
/// publishes; an import file that is the same file is refused, because the
/// harness appends to it.
pub fn load(
    root: &Path,
    cells: &[SelectedCell],
    context: &RunContext,
    results_path: &Path,
    policy: &ImportPolicy<'_>,
) -> Result<ImportedRun, String> {
    let wanted = cells.iter().map(cell_key).collect::<BTreeSet<_>>();
    let buckets = cells
        .iter()
        .map(|cell| (cell.test.lane.clone(), cell.category.clone()))
        .collect::<BTreeSet<_>>();
    let publish = fs::canonicalize(results_path).ok();
    let mut rows = BTreeMap::<Key, Vec<CellResult>>::new();
    let mut inapplicable = BTreeMap::<Key, String>::new();
    let mut evidence_complete = BTreeSet::<(Key, String)>::new();
    for (lane, category) in &buckets {
        let dir = bucket_dir(root, lane, category);
        let results = dir.join("results.jsonl");
        if publish.is_some() && fs::canonicalize(&results).ok() == publish {
            return Err(format!(
                "{IMPORT_RESULTS_ENV} file {} is this run's own results path",
                results.display()
            ));
        }
        match fs::read_to_string(&results) {
            Ok(text) => {
                for (number, line) in text.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let row: CellResult = serde_json::from_str(line).map_err(|error| {
                        format!("{}:{}: {error}", results.display(), number + 1)
                    })?;
                    if row.lane != *lane || row.category != *category {
                        return Err(format!(
                            "{}:{}: row of {}/{} in the {lane}/{category} bucket",
                            results.display(),
                            number + 1,
                            row.lane,
                            row.category
                        ));
                    }
                    let id = key(lane, category, &row.test, &row.mode, row.backend.as_deref());
                    if wanted.contains(&id) {
                        rows.entry(id).or_default().push(row);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", results.display())),
        }
        let summary = dir.join("summary.json");
        match fs::read(&summary) {
            Ok(bytes) => {
                let summary: Summary = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("{}: {error}", summary.display()))?;
                for cell in summary.host_inapplicable_cells {
                    let id = key(
                        lane,
                        category,
                        &cell.test,
                        &cell.mode,
                        cell.backend.as_deref(),
                    );
                    if wanted.contains(&id) {
                        inapplicable.insert(id, cell.reason.unwrap_or_default());
                    }
                }
                evidence_complete.extend(summary.evidence_complete_executions.into_iter().map(
                    |execution| {
                        let id = key(
                            lane,
                            category,
                            &execution.test,
                            &execution.mode,
                            execution.backend.as_deref(),
                        );
                        (id, execution.run_id)
                    },
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", summary.display())),
        }
    }

    let mut source_run_ids = BTreeSet::new();
    let mut missing = 0;
    let mut dropped_retries = 0;
    let cells = cells
        .iter()
        .map(|cell| {
            let id = cell_key(cell);
            let found = rows.remove(&id);
            let claimed = inapplicable.remove(&id);
            let rows = match (found, claimed) {
                (Some(_), Some(_)) => vec![error_row(
                    context,
                    cell,
                    "import-conflict",
                    format!(
                        "{IMPORT_RESULTS_ENV} has both result rows and a host-inapplicable entry for this cell"
                    ),
                )],
                (None, Some(claimed)) => match (policy.host_inapplicable)(cell) {
                    Some(reason) => vec![host_inapplicable_result(context, cell, reason)],
                    None => vec![error_row(
                        context,
                        cell,
                        "import-host-inapplicable-unconfirmed",
                        format!(
                            "{IMPORT_RESULTS_ENV} says this cell could not run ({claimed:?}), but this machine has every capability it requires"
                        ),
                    )],
                },
                (None, None) => {
                    missing += 1;
                    vec![error_row(
                        context,
                        cell,
                        "import-missing",
                        format!(
                            "{IMPORT_RESULTS_ENV}={} has no result for this selected cell",
                            root.display()
                        ),
                    )]
                }
                (Some(mut found), None) => {
                    if let Some(reason) = found.iter().find_map(|row| stale_reason(context, cell, row)) {
                        vec![error_row(context, cell, "import-stale", reason)]
                    } else if let Some(reason) = history_error(&found) {
                        vec![error_row(context, cell, "import-history", reason)]
                    } else {
                        // Keep the history up to the first attempt this run
                        // would not have retried. A PASS is already the last
                        // row (`history_error`), so only a failed attempt can
                        // drop later ones. A FAIL or an ERROR continues it
                        // exactly when the policy retries it: a typed skid
                        // overshoot does, as it would had this run executed
                        // the attempt, because the policy re-decides it from
                        // the report bytes the row retains.
                        let kept = found
                            .iter()
                            .position(|row| row.outcome == "PASS" || !(policy.earns_retry)(cell, row))
                            .map_or(found.len(), |terminal| terminal + 1);
                        dropped_retries += found.len() - kept;
                        found.truncate(kept);
                        let last = found.last().expect("a found cell has at least one row");
                        if last.outcome == "PASS"
                            && !evidence_complete.contains(&(id.clone(), last.run_id.clone()))
                        {
                            vec![error_row(
                                context,
                                cell,
                                "import-evidence-incomplete",
                                format!(
                                    "{IMPORT_RESULTS_ENV} has a PASS for this cell from producer run {}, which has no evidence_complete_executions record",
                                    last.run_id
                                ),
                            )]
                        } else {
                            found
                                .into_iter()
                                .map(|mut row| {
                                    source_run_ids.insert(std::mem::replace(
                                        &mut row.run_id,
                                        context.run_id.clone(),
                                    ));
                                    row.source_tree_dirty |= context.source_dirty;
                                    if let Some(observations) = row.cpu_observations.as_mut() {
                                        observations.binding.run_id = context.run_id.clone();
                                        observations.binding.outer_attempt = row.attempt;
                                    }
                                    row
                                })
                                .collect()
                        }
                    }
                }
            };
            ImportedCell { rows }
        })
        .collect();
    Ok(ImportedRun {
        cells,
        source_run_ids,
        missing,
        dropped_retries,
    })
}
