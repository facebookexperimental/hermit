// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the per-cell result population carried by one full validate run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use hermit_manifest_plan::canonical_verdict::InfrastructureError;
use hermit_manifest_plan::canonical_verdict::Verdict as VerificationVerdict;
use hermit_manifest_plan::canonical_verdict::VerificationReport;
use hermit_manifest_plan::ledger::CellIdentity;
use hermit_manifest_plan::ledger::CellResult as LedgerCellResult;
use hermit_manifest_plan::ledger::CellResultsArtifact;
use hermit_manifest_plan::ledger::CellResultsEvidence;
use hermit_manifest_plan::ledger::CellVerdict;
use hermit_manifest_plan::ledger::ComparedLogCounts;
use hermit_manifest_plan::ledger::ComparisonSpec;
use hermit_manifest_plan::ledger::ComparisonTier;
use hermit_manifest_plan::ledger::RequiredNullable;
use hermit_manifest_plan::runner::outcome_after_retries;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

/// Outer ledger schema for rows carrying [`RetainedCellResults`].
///
/// Schema 6 contains two historical rows written before the current comparison
/// fields existed. Schema 7 guarantees that every compared verdict carries the
/// complete current comparison object; a missing or additional field is kept
/// out of the compared-verdict projection instead of changing this shape under
/// the same version.
pub const CELL_RESULTS_LEDGER_SCHEMA_MIN: i64 = 6;
pub const CELL_RESULTS_LEDGER_SCHEMA_VERSION: i64 = 7;

#[derive(Debug)]
pub struct RetainedCellResults {
    pub schema_version: i64,
    pub run_id: String,
    /// The surrounding validation row is assembled as JSON, but this value is
    /// always serialized from the producer-owned [`CellResultsEvidence`]
    /// contract rather than constructed as a second untyped definition.
    pub evidence: Value,
}

#[derive(Debug)]
pub struct RetainedCoverageEvidence {
    pub evidence: Value,
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn collect_results_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read per-cell result root {}: {error}",
            path.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read per-cell result entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot classify {}: {error}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_results_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == "results.jsonl" {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn read_result_rows(path: &Path) -> Result<Vec<(PathBuf, usize, Value)>, String> {
    read_result_rows_with(path, false)
}

fn read_result_rows_with(path: &Path, schema10: bool) -> Result<Vec<(PathBuf, usize, Value)>, String> {
    let mut files = Vec::new();
    collect_results_files(path, &mut files)?;
    files.sort();
    let mut rows = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let parsed = if schema10 {
                hermit_manifest_plan::ledger::read_schema10_source_result(line.as_bytes())
            } else {
                serde_json::from_str(line).map_err(|error| error.to_string())
            };
            let row = parsed.map_err(|error| {
                format!(
                    "{}:{} malformed result row: {error}",
                    file.display(),
                    line_number + 1
                )
            })?;
            rows.push((file.clone(), line_number + 1, row));
        }
    }
    Ok(rows)
}

/// Read every retained cell attempt from the harness's appended result files.
///
/// This is the same population `retain` validates. Keeping one reader prevents
/// the history writer from silently omitting retries that the terminal-verdict
/// projection deliberately reduces to the latest attempt.
pub fn all_result_rows(path: &Path) -> Result<Vec<Value>, String> {
    read_result_rows(path).map(|rows| rows.into_iter().map(|(_, _, row)| row).collect())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("per-cell result has no nonempty {key}"))
}
fn preserved_reason<'a>(row: &'a Value, attempt: Option<&'a Value>) -> Option<&'a str> {
    attempt
        .and_then(|value| value.get("reason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty())
        .or_else(|| {
            row.get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
        })
}

fn identity(value: &Value) -> Result<CellIdentity, String> {
    Ok(CellIdentity {
        lane: string(value, "lane")?.into(),
        category: string(value, "category")?.into(),
        test: string(value, "test")?.into(),
        mode: string(value, "mode")?.into(),
        backend: string(value, "backend")?.into(),
    })
}

fn identity_value(identity: &CellIdentity) -> Result<Value, String> {
    serde_json::to_value(identity)
        .map_err(|error| format!("cannot encode selected cell identity: {error}"))
}

fn require_current_timeout_policy(row: &Value) -> Result<(), String> {
    let timeout = row
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .ok_or("current cell result has no timeout_seconds")?;
    let cpu = row
        .get("execution_cpu_timeout_seconds")
        .and_then(Value::as_u64)
        .ok_or("current cell result omitted execution_cpu_timeout_seconds")?;
    let wall = row
        .get("execution_wall_timeout_seconds")
        .and_then(Value::as_u64)
        .ok_or("current cell result omitted execution_wall_timeout_seconds")?;
    if cpu == 0 || wall != timeout || wall <= cpu {
        return Err(format!(
            "current cell result timeout policy disagrees: timeout_seconds={timeout} execution_cpu_timeout_seconds={cpu} execution_wall_timeout_seconds={wall}"
        ));
    }
    Ok(())
}

fn canonical_report(
    value: Value,
    bytes: &[u8],
    expected_virtualize_time: bool,
) -> Result<Option<(VerificationReport, ComparisonSpec, ComparedLogCounts)>, String> {
    // `VerificationReport` owns the complete current top-level report. The
    // ledger types additionally deny unknown comparison/count fields, which
    // preserves schema 7's exact shape without a second hard-coded key list.
    let report = VerificationReport::from_current_json_slice(bytes)?;
    if report.verdict == VerificationVerdict::InfrastructureError {
        return Err(match report.infrastructure_error.as_ref() {
            Some(InfrastructureError::SkidOvershoot { count }) => format!(
                "recorded infrastructure_error: {count} HERMIT_SKID_OVERSHOOT report(s)"
            ),
            None => unreachable!("typed report parser requires an infrastructure error"),
        });
    }
    let comparison = value
        .get("comparison")
        .cloned()
        .ok_or("incomplete cell comparison: missing `comparison`")?;
    let comparison = serde_json::from_value::<ComparisonSpec>(comparison)
        .map_err(|error| format!("incomplete cell comparison: {error}"))?;
    let compared_log_messages = serde_json::from_value::<RequiredNullable<ComparedLogCounts>>(
        value
            .get("compared_log_messages")
            .cloned()
            .ok_or("incomplete cell comparison: missing `compared_log_messages`")?,
    )
    .map_err(|error| format!("incomplete cell comparison counts: {error}"))?;
    if !comparison.is_canonical_bitwise_info_v1_for_time_policy(
        expected_virtualize_time,
        &compared_log_messages,
    ) {
        return Ok(None);
    }
    let RequiredNullable::Value(compared_log_messages) = compared_log_messages else {
        return Ok(None);
    };
    Ok(Some((report, comparison, compared_log_messages)))
}

fn cell_verdict(row: &Value) -> Result<CellVerdict, String> {
    let mode = string(row, "mode")?;
    if mode == "naked" || mode == "custom" {
        return Ok(CellVerdict::PerformsNoComparisonByDesign {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: format!("{mode} mode does not perform canonical two-run comparison"),
        });
    }
    // Verify and chaos compare independent executions and require virtual
    // time. Replay compares one recording with its replay and deliberately
    // leaves time real. Bind the receipt to that producer policy: accepting
    // either boolean would weaken the comparison requirement.
    let expected_virtualize_time = match mode {
        "replay" => false,
        "verify" | "chaos" => true,
        _ => {
            return Err(format!(
                "unsupported cell mode `{mode}` has no declared canonical comparison policy"
            ));
        }
    };
    let Some(attempts) = row.get("attempts").and_then(Value::as_array) else {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: preserved_reason(row, None)
                .unwrap_or("cell emitted no typed attempts")
                .into(),
        });
    };
    let mut reports = Vec::new();
    let mut unavailable_reason = None;
    for (index, attempt) in attempts.iter().enumerate() {
        let preserved_reason = preserved_reason(row, Some(attempt)).map(str::to_owned);
        let Some(raw) = attempt.get("verification_report").and_then(Value::as_str) else {
            unavailable_reason = Some(preserved_reason.clone().unwrap_or_else(|| {
                format!(
                    "attempt {} emitted no typed verification report",
                    index + 1
                )
            }));
            continue;
        };
        let expected_sha = attempt
            .get("verification_report_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("attempt {} omitted verification_report_sha256", index + 1))?;
        if hex_digest(raw.as_bytes()) != expected_sha {
            return Err(format!(
                "attempt {} verification_report_sha256 mismatch",
                index + 1
            ));
        }
        let value = serde_json::from_str::<Value>(raw).map_err(|error| {
            format!(
                "attempt {} verification report is malformed: {error}",
                index + 1
            )
        })?;
        match canonical_report(value, raw.as_bytes(), expected_virtualize_time) {
            Ok(Some(report)) => reports.push(report),
            Ok(None) => {
                unavailable_reason = Some(preserved_reason.clone().unwrap_or_else(|| {
                    format!(
                        "attempt {} did not compare canonical nonzero INFO evidence",
                        index + 1
                    )
                }));
            }
            Err(error) => {
                unavailable_reason = Some(
                    preserved_reason.unwrap_or_else(|| format!("attempt {} {error}", index + 1)),
                )
            }
        }
    }
    let classify = |(report, _, _): &(VerificationReport, ComparisonSpec, ComparedLogCounts)| {
        let matched = report.verified
            && report.verdict == VerificationVerdict::Matched
            && report.bitwise_parity;
        let diverged = report.verdict == VerificationVerdict::Diverged && !report.bitwise_parity;
        (matched, diverged)
    };
    // A genuine canonical divergence is sticky across sibling attempts. Missing
    // or weaker evidence may prevent a clean leg, but it must never erase a red
    // leg merely because it was observed before or after that divergence.
    if let Some((_, comparison, compared_log_messages)) =
        reports.iter().find(|report| classify(report).1)
    {
        return Ok(CellVerdict::ComparedAndDiverged {
            comparison_tier: ComparisonTier::CanonicalBitwise,
            comparison: comparison.clone(),
            bitwise_parity: false,
            compared_log_messages: RequiredNullable::Value(compared_log_messages.clone()),
        });
    }
    if reports.is_empty() || unavailable_reason.is_some() {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: unavailable_reason.unwrap_or_else(|| {
                preserved_reason(row, None)
                    .unwrap_or("cell emitted no typed verification report")
                    .into()
            }),
        });
    }
    if reports.iter().any(|report| !classify(report).0) {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: "typed canonical report was neither a match nor a divergence".into(),
        });
    }
    if string(row, "outcome")? != "PASS" {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: row
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("cell outcome was not PASS despite matched comparison evidence")
                .into(),
        });
    }
    let (_, comparison, compared_log_messages) = reports.last().expect("nonempty reports");
    Ok(CellVerdict::ComparedAndMatched {
        comparison_tier: ComparisonTier::CanonicalBitwise,
        comparison: comparison.clone(),
        bitwise_parity: true,
        compared_log_messages: RequiredNullable::Value(compared_log_messages.clone()),
    })
}

fn sort_key(value: &Value) -> Result<CellIdentity, String> {
    identity(value)
}

pub fn expected_plan(repo_root: &Path) -> Result<Vec<Value>, String> {
    let path = repo_root.join("ci/expected-e2e-plan.json");
    let document: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read expected plan {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("expected plan {} is malformed: {error}", path.display()))?;
    document
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("expected plan {} has no cells array", path.display()))?
        .iter()
        .map(|cell| identity(cell).and_then(|identity| identity_value(&identity)))
        .collect()
}

fn string_set(value: &Value, key: &str) -> Result<BTreeSet<String>, String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("test-binary registration has no {key} array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("test-binary registration {key} contains a non-name"))
        })
        .collect()
}

fn enabled_cell_scope(cell: &Value) -> Result<Value, String> {
    let mut scoped = identity_value(&identity(cell)?)?
        .as_object()
        .cloned()
        .ok_or("cell identity was not an object")?;
    for key in ["status", "measurement", "reason", "last_tested", "observations"] {
        if let Some(value) = cell.get(key) {
            scoped.insert(key.into(), value.clone());
        }
    }
    let mut passes = 0u64;
    let mut failures = 0u64;
    let mut other = BTreeMap::<String, u64>::new();
    for result in cell
        .get("observations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|observation| observation.get("results").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
    {
        match result {
            "pass" => passes += 1,
            "fail" => failures += 1,
            value => *other.entry(value.to_string()).or_default() += 1,
        }
    }
    scoped.insert("observed_pass_count".into(), serde_json::json!(passes));
    scoped.insert("observed_fail_count".into(), serde_json::json!(failures));
    scoped.insert("observed_other_results".into(), serde_json::json!(other));
    Ok(Value::Object(scoped))
}

fn coverage_document(
    plan_name: &str,
    selection_mode: &str,
    planned_nodes: &BTreeSet<String>,
    planned_test_nodes: &BTreeSet<String>,
    test_node_coverage: &Value,
    selected: &[Value],
    cells_document: &Value,
    registration: &Value,
) -> Result<Value, String> {
    let selected: BTreeMap<_, _> = selected
        .iter()
        .map(|cell| Ok((sort_key(cell)?, cell.clone())))
        .collect::<Result<_, String>>()?;
    let enabled: BTreeMap<_, _> = cells_document
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("ci/compat-envelope/cells.json has no cells array")?
        .iter()
        .filter(|cell| cell.get("enabled").and_then(Value::as_bool) == Some(true))
        .map(|cell| {
            let id = identity(cell)?;
            Ok((id, enabled_cell_scope(cell)?))
        })
        .collect::<Result<_, String>>()?;
    let selected_and_enabled = selected.keys().filter(|key| enabled.contains_key(*key)).count();
    let enabled_not_selected: Vec<Value> = enabled
        .iter()
        .filter(|(key, _)| !selected.contains_key(*key))
        .map(|(_, value)| value.clone())
        .collect();
    let selected_not_enabled: Vec<Value> = selected
        .iter()
        .filter(|(key, _)| !enabled.contains_key(*key))
        .map(|(_, value)| value.clone())
        .collect();

    if registration.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("test-binary registration has unsupported schema".into());
    }
    let present = string_set(registration, "present")?;
    let ci_registered = string_set(registration, "ci_registered")?;
    let none_recorded = string_set(registration, "none_recorded")?;
    let undeclared = string_set(registration, "undeclared")?;
    let reason_rows = registration
        .get("reason_recorded")
        .and_then(Value::as_array)
        .ok_or("test-binary registration has no reason_recorded array")?;
    let reason_recorded: BTreeSet<String> = reason_rows
        .iter()
        .map(|row| string(row, "binary").map(str::to_string))
        .collect::<Result<_, _>>()?;
    let accounted: BTreeSet<String> = ci_registered
        .iter()
        .chain(reason_recorded.iter())
        .chain(none_recorded.iter())
        .chain(undeclared.iter())
        .cloned()
        .collect();
    if accounted != present
        || !ci_registered.is_disjoint(&reason_recorded)
        || !ci_registered.is_disjoint(&none_recorded)
        || !ci_registered.is_disjoint(&undeclared)
        || !reason_recorded.is_disjoint(&none_recorded)
        || !reason_recorded.is_disjoint(&undeclared)
        || !none_recorded.is_disjoint(&undeclared)
    {
        return Err("test-binary registration does not form an exact partition".into());
    }

    Ok(serde_json::json!({
        "schema": 1,
        "plan": {
            "name": plan_name,
            "selection_mode": selection_mode,
            "outer_node_count": planned_nodes.len(),
            "outer_nodes": planned_nodes,
        },
        "test_nodes": {
            "planned": planned_test_nodes,
            "coverage": test_node_coverage,
        },
        "e2e": {
            "selected_count": selected.len(),
            "enabled_count": enabled.len(),
            "selected_and_enabled_count": selected_and_enabled,
            "enabled_not_selected_count": enabled_not_selected.len(),
            "selected_not_enabled_count": selected_not_enabled.len(),
            "selected": selected.into_values().collect::<Vec<_>>(),
            "enabled_not_selected": enabled_not_selected,
            "selected_not_enabled": selected_not_enabled,
        },
        "integration_test_binaries": registration,
    }))
}

/// Retain the exact test population around a full run, including work outside
/// the selected set. This is reporting, not an exemption: a reader can tell a
/// selected cell from an enabled cell that ordinary validation never selected,
/// and can see every integration-test binary outside the CI DAG.
pub fn retain_coverage_evidence(
    parent: &Path,
    repo_root: &Path,
    run_id: &str,
    commit: &str,
    plan_name: &str,
    selection_mode: &str,
    planned_nodes: &BTreeSet<String>,
    planned_test_nodes: &BTreeSet<String>,
    test_node_coverage: &Value,
    selected: &[Value],
) -> Result<RetainedCoverageEvidence, String> {
    let cells_path = repo_root.join("ci/compat-envelope/cells.json");
    let cells_document: Value = serde_json::from_slice(
        &fs::read(&cells_path)
            .map_err(|error| format!("cannot read {}: {error}", cells_path.display()))?,
    )
    .map_err(|error| format!("{} is malformed: {error}", cells_path.display()))?;
    let audit = repo_root.join("ci/audit-test-binary-registration.py");
    let output = std::process::Command::new("python3")
        .arg(&audit)
        .arg("--root")
        .arg(repo_root)
        .arg("--json")
        .output()
        .map_err(|error| format!("cannot execute {}: {error}", audit.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --json exited {}; {}",
            audit.display(),
            output.status.code().map_or_else(|| "by signal".into(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let registration: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("test-binary registration JSON is malformed: {error}"))?;
    let document = coverage_document(
        plan_name,
        selection_mode,
        planned_nodes,
        planned_test_nodes,
        test_node_coverage,
        selected,
        &cells_document,
        &registration,
    )?;
    let artifact_dir = parent.join("ignored").join("validate").join("artifacts").join(run_id);
    fs::create_dir_all(&artifact_dir).map_err(|error| {
        format!("cannot create retained coverage directory {}: {error}", artifact_dir.display())
    })?;
    let artifact = artifact_dir.join("coverage.json");
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("cannot encode coverage evidence: {error}"))?;
    bytes.push(b'\n');
    fs::write(&artifact, &bytes)
        .map_err(|error| format!("cannot publish {}: {error}", artifact.display()))?;
    let relative = artifact
        .strip_prefix(parent)
        .map_err(|_| "retained coverage artifact is outside parent root")?
        .to_string_lossy()
        .into_owned();
    let e2e = document.get("e2e").expect("constructed e2e scope");
    let binaries = document
        .get("integration_test_binaries")
        .expect("constructed integration-test scope");
    let mut evidence = test_node_coverage
        .as_object()
        .cloned()
        .ok_or("test-node coverage was not an object")?;
    evidence.insert("schema".into(), serde_json::json!(1));
    evidence.insert("run_id".into(), serde_json::json!(run_id));
    evidence.insert("hermit_sha".into(), serde_json::json!(commit));
    evidence.insert("plan".into(), document["plan"].clone());
    evidence.insert("test_nodes".into(), document["test_nodes"].clone());
    evidence.insert(
        "e2e".into(),
        serde_json::json!({
            "selected_count": e2e["selected_count"],
            "enabled_count": e2e["enabled_count"],
            "selected_and_enabled_count": e2e["selected_and_enabled_count"],
            "enabled_not_selected_count": e2e["enabled_not_selected_count"],
            "selected_not_enabled_count": e2e["selected_not_enabled_count"],
        }),
    );
    evidence.insert(
        "integration_test_binaries".into(),
        serde_json::json!({
            "present_count": binaries["present"].as_array().map_or(0, Vec::len),
            "ci_registered_count": binaries["ci_registered"].as_array().map_or(0, Vec::len),
            "reason_recorded_count": binaries["reason_recorded"].as_array().map_or(0, Vec::len),
            "none_recorded_count": binaries["none_recorded"].as_array().map_or(0, Vec::len),
            "undeclared_count": binaries["undeclared"].as_array().map_or(0, Vec::len),
        }),
    );
    evidence.insert(
        "artifact".into(),
        serde_json::json!({
            "path": relative,
            "sha256": hex_digest(&bytes),
        }),
    );
    Ok(RetainedCoverageEvidence { evidence: Value::Object(evidence) })
}

/// Transform all result rows for one validate invocation into the closed
/// schema-7 cell-verdict artifact and summary used by ci-hub.
pub fn retain(
    parent: &Path,
    result_root: &Path,
    commit: &str,
    expected: &[Value],
) -> Result<RetainedCellResults, String> {
    let mut run_id: Option<String> = None;
    let mut selected = Vec::new();
    let mut identities = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut attempt_rows: BTreeMap<CellIdentity, Vec<(u64, Value)>> = BTreeMap::new();
    for (file, line_number, row) in read_result_rows(result_root)? {
            if row.get("schema").and_then(Value::as_u64) != Some(4)
                || string(&row, "hermit_sha")? != commit
                || row.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
            {
                return Err(format!(
                    "{}:{line_number} is not an exact clean schema-4 cell result for {commit}",
                    file.display()
                ));
            }
            require_current_timeout_policy(&row)
                .map_err(|error| format!("{}:{line_number} {error}", file.display()))?;
            let row_run_id = string(&row, "run_id")?;
            match run_id.as_deref() {
                None => run_id = Some(row_run_id.into()),
                Some(existing) if existing == row_run_id => {}
                Some(existing) => {
                    return Err(format!(
                        "per-cell results mix run_id {existing} with {row_run_id}"
                    ));
                }
            }
            let id = identity(&row)?;
            let key = id.clone();
            let attempt = row.get("attempt").and_then(Value::as_u64).unwrap_or(1);
            if attempt == 0 {
                return Err("per-cell result attempt must be positive".into());
            }
            if !observations.insert((key.clone(), attempt)) {
                return Err("per-cell results contain a duplicate identity and attempt".into());
            }
            if identities.insert(key.clone()) {
                selected.push(id);
            }
            attempt_rows.entry(key).or_default().push((attempt, row));
    }
    let mut cells = attempt_rows
        .into_iter()
        .map(|(identity, mut rows)| {
            rows.sort_by_key(|(attempt, _)| *attempt);
            let outcome = outcome_after_retries(rows.iter().map(|(attempt, row)| {
                Ok((*attempt, string(row, "outcome")?))
            }).collect::<Result<Vec<_>, String>>()?)?;
            let row = rows
                .iter()
                .rev()
                .find(|(_, row)| row.get("outcome").and_then(Value::as_str) == Some(outcome))
                .map(|(_, row)| row)
                .ok_or_else(|| {
                    format!(
                        "cell result history selected {outcome} without a matching row for {identity:?}"
                    )
                })?;
            Ok(LedgerCellResult {
                lane: identity.lane,
                category: identity.category,
                test: identity.test,
                mode: identity.mode,
                backend: identity.backend,
                cell_verdict: cell_verdict(row)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let run_id = run_id.ok_or("full validation retained zero per-cell result rows")?;
    selected.sort();
    cells.sort_by_key(|cell| CellIdentity {
        lane: cell.lane.clone(),
        category: cell.category.clone(),
        test: cell.test.clone(),
        mode: cell.mode.clone(),
        backend: cell.backend.clone(),
    });
    let mut expected = expected
        .iter()
        .map(identity)
        .collect::<Result<Vec<_>, String>>()?;
    expected.sort();
    if selected != expected {
        let observed_keys = selected.iter().collect::<BTreeSet<_>>();
        let expected_keys = expected.iter().collect::<BTreeSet<_>>();
        let missing = expected_keys.difference(&observed_keys).count();
        let extra = observed_keys.difference(&expected_keys).count();
        return Err(format!(
            "per-cell results differ from the exact planned population: {missing} missing, {extra} extra"
        ));
    }
    let selected_values = selected
        .iter()
        .map(identity_value)
        .collect::<Result<Vec<_>, String>>()?;
    let population_bytes = serde_json::to_vec(&selected_values)
        .map_err(|error| format!("cannot encode selected cell population: {error}"))?;
    let artifact_dir = parent
        .join("ignored")
        .join("validate")
        .join("artifacts")
        .join(&run_id);
    fs::create_dir_all(&artifact_dir).map_err(|error| {
        format!(
            "cannot create retained cell artifact {}: {error}",
            artifact_dir.display()
        )
    })?;
    let artifact = artifact_dir.join("cell-results.jsonl");
    let mut artifact_bytes = Vec::new();
    for cell in &cells {
        let mut record = serde_json::to_value(cell)
            .map_err(|error| format!("cannot encode retained cell row: {error}"))?
            .as_object()
            .cloned()
            .ok_or("shared cell result did not serialize as an object")?;
        record.insert("run_id".into(), Value::String(run_id.clone()));
        record.insert("hermit_sha".into(), Value::String(commit.into()));
        record.insert("source_tree_dirty".into(), Value::Bool(false));
        serde_json::to_writer(&mut artifact_bytes, &record)
            .map_err(|error| format!("cannot encode retained cell row: {error}"))?;
        artifact_bytes.push(b'\n');
    }
    fs::write(&artifact, &artifact_bytes).map_err(|error| {
        format!(
            "cannot publish retained cell artifact {}: {error}",
            artifact.display()
        )
    })?;
    let relative = artifact
        .strip_prefix(parent)
        .map_err(|_| "retained cell artifact is outside parent root")?
        .to_string_lossy()
        .into_owned();
    let recorded_count = u64::try_from(cells.len())
        .map_err(|_| "retained cell count does not fit the ledger type")?;
    let selected_count = u64::try_from(selected.len())
        .map_err(|_| "selected cell count does not fit the ledger type")?;
    let evidence = CellResultsEvidence {
        run_id: run_id.clone(),
        hermit_sha: commit.into(),
        source_tree_dirty: false,
        selected_count,
        recorded_count,
        population_sha256: hex_digest(&population_bytes),
        artifact: CellResultsArtifact {
            path: relative,
            sha256: hex_digest(&artifact_bytes),
            row_count: recorded_count,
        },
        selected,
        cells,
    };
    Ok(RetainedCellResults {
        schema_version: CELL_RESULTS_LEDGER_SCHEMA_VERSION,
        run_id,
        evidence: serde_json::to_value(evidence)
            .map_err(|error| format!("cannot encode cell_results evidence: {error}"))?,
    })
}

/// Retain full parity attempts in the immutable artifact and only derived,
/// path-free summaries in schema 10. Ordinary and parity outcomes remain
/// separate, and the selected population comes from the pre-execution plan.
pub fn retain_v10(
    parent: &Path,
    result_root: &Path,
    plan: &hermit_manifest_plan::ledger::ConstructedValidationPlanV10,
) -> Result<RetainedCellResults, String> {
    use hermit_manifest_plan::ledger::{CellArtifactResultV10, CellBackendParity, CellResultsEvidenceV10};
    let selected = plan.planned_cells()?;
    let selected_set = selected.iter().cloned().collect::<BTreeSet<_>>();
    let selected_backend_parity = plan.planned_backend_parity_relations()?;
    let parity_candidates = selected_backend_parity.iter().map(|relation| relation.candidate.clone()).collect::<BTreeSet<_>>();
    let mut rows = BTreeMap::<CellIdentity, Vec<(u64, Value)>>::new();
    let mut seen = BTreeSet::new();
    for (file, line, row) in read_result_rows_with(result_root, true)? {
        if row.get("schema").and_then(Value::as_u64) != Some(4)
            || string(&row, "hermit_sha")? != plan.hermit_sha
            || string(&row, "run_id")? != plan.run_id
            || row.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
        {
            return Err(format!("{}:{line} is not a clean current result for the retained plan", file.display()));
        }
        require_current_timeout_policy(&row)?;
        let id = identity(&row)?;
        let attempt = row.get("attempt").and_then(Value::as_u64).ok_or("current cell result omitted attempt")?;
        if !selected_set.contains(&id) || !seen.insert((id.clone(), attempt)) {
            return Err("schema 10 results contain an unselected cell or repeated attempt".into());
        }
        rows.entry(id).or_default().push((attempt, row));
    }
    let mut full_cells = Vec::new();
    for (id, mut rows) in rows {
        rows.sort_by_key(|(attempt, _)| *attempt);
        let (cell_verdict, backend_parity) = if parity_candidates.contains(&id) {
            let parity = CellBackendParity::from_result_rows(&id, &rows)?;
            (parity.candidate_verdict(&id)?, RequiredNullable::Value(parity))
        } else {
            if rows.iter().any(|(_, row)| row.get("backend_parity").is_some_and(|value| !value.is_null())
                || row.get("attempts").and_then(Value::as_array).is_some_and(|attempts| {
                    attempts.iter().any(|attempt| attempt.get("index").and_then(Value::as_str) == Some("parity-reference"))
                }))
            {
                return Err("ordinary selected cell emitted an unplanned parity comparison".into());
            }
            let outcome = outcome_after_retries(rows.iter().map(|(attempt, row)| Ok((*attempt, string(row, "outcome")?)))
                .collect::<Result<Vec<_>, String>>()?)?;
            let row = rows.iter().rev().find(|(_, row)| row.get("outcome").and_then(Value::as_str) == Some(outcome))
                .map(|(_, row)| row).ok_or("ordinary cell has no selected terminal result")?;
            (cell_verdict(row)?, RequiredNullable::Null)
        };
        full_cells.push(CellArtifactResultV10 { lane: id.lane, category: id.category, test: id.test,
            mode: id.mode, backend: id.backend, cell_verdict, backend_parity });
    }
    let mut bytes = Vec::new();
    let mut cells = Vec::new();
    for cell in &full_cells {
        cells.push(cell.summary()?);
        let mut row = serde_json::to_value(cell).map_err(|error| error.to_string())?;
        let object = row.as_object_mut().ok_or("schema 10 full cell is not an object")?;
        object.insert("run_id".into(), Value::String(plan.run_id.clone()));
        object.insert("hermit_sha".into(), Value::String(plan.hermit_sha.clone()));
        object.insert("source_tree_dirty".into(), Value::Bool(false));
        serde_json::to_writer(&mut bytes, &row).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
    }
    let population = serde_json::to_vec(&serde_json::to_value(&selected).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    let artifact_path = super::validate_artifacts::publish_run_artifact_noclobber(
        parent, &plan.run_id, "cell-results.jsonl", &bytes, "retained full cell results")?;
    let evidence = CellResultsEvidenceV10 {
        path: plan.path, run_id: plan.run_id.clone(), hermit_sha: plan.hermit_sha.clone(), source_tree_dirty: false,
        selected_count: selected.len() as u64, recorded_count: cells.len() as u64,
        population_sha256: hex_digest(&population), artifact: CellResultsArtifact {
            path: artifact_path, sha256: hex_digest(&bytes), row_count: cells.len() as u64 },
        selected, selected_backend_parity, cells,
    };
    evidence.verify_cell_artifact_bytes(&bytes)?;
    Ok(RetainedCellResults { schema_version: 10, run_id: plan.run_id.clone(),
        evidence: serde_json::to_value(evidence).map_err(|error| error.to_string())? })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture_root() -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("validate-cell-results-{}-{id}", std::process::id()))
    }

    fn report_with_virtualize_time(
        verdict: &str,
        log_scope: &str,
        virtualize_time: bool,
    ) -> String {
        let matched = verdict == "matched";
        let output = serde_json::json!({"exit_code": 0, "signal": null,
            "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "stdout_bytes": 0,
            "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "stderr_bytes": 0});
        serde_json::json!({
            "verified": matched,
            "verdict": verdict,
            "bitwise_parity": matched,
            "no_result_reason": null,
            "infrastructure_error": null,
            "comparison": {
                "strictness": "canonical",
                "display_name": "BitwiseInfoV1",
                "compare_logs": true,
                "compare_io_buffers": true,
                "log_scope": log_scope,
                "record_envelope": "all_records_v1",
                "virtualize_time": virtualize_time,
                "strip_lines": false,
                "canonicalize_addresses": true,
                "full_trace": true,
                "exact_remainder": true,
                "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines": false,
                "skip_commit": false,
                "skip_detlog": false
            },
            "compared_outputs": {"left": output, "right": output},
            "compared_log_messages": {"left": 123, "right": if matched { 123 } else { 124 }},
            "guest_exit_code": 0,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        })
        .to_string()
    }

    fn report(verdict: &str, log_scope: &str) -> String {
        report_with_virtualize_time(verdict, log_scope, true)
    }

    fn replace_report(row: &mut Value, report: &Value) {
        let raw = serde_json::to_string(report).unwrap();
        row["attempts"][0] = attempt(&raw);
    }

    fn attempt(raw: &str) -> Value {
        serde_json::json!({
            "verification_report": raw,
            "verification_report_sha256": hex_digest(raw.as_bytes())
        })
    }

    fn result_row(run_id: &str, commit: &str) -> Value {
        let matched = report("matched", "info");
        serde_json::json!({
            "schema": 4,
            "run_id": run_id,
            "hermit_sha": commit,
            "source_tree_dirty": false,
            "lane": "portable",
            "category": "c-programs",
            "test": "uname",
            "mode": "verify",
            "backend": "ptrace",
            "outcome": "PASS",
            "reason": null,
            "timeout_seconds": 57,
            "execution_cpu_timeout_seconds": 22,
            "execution_wall_timeout_seconds": 57,
            "attempts": [attempt(&matched)]
        })
    }

    #[test]
    fn cumulative_writer_preserves_reference_and_cross_failures_separately() {
        use hermit_manifest_plan::ledger::ConstructedValidationPlanV10;
        let plan: ConstructedValidationPlanV10 = serde_json::from_str(
            include_str!("fixtures/schema10-matched-plan.json")).unwrap();
        let retained: Value = serde_json::from_str(
            include_str!("fixtures/schema10-matched-cell.json")).unwrap();
        let completed = &retained["backend_parity"]["attempts"][0];
        let base = serde_json::json!({
            "schema":4, "run_id":plan.run_id, "hermit_sha":plan.hermit_sha,
            "source_tree_dirty":false, "attempt":1,
            "test":retained["test"], "category":retained["category"],
            "lane":retained["lane"], "mode":"verify", "backend":"kvm",
            "classification":"deterministic", "outcome":"PASS", "result":"pass",
            "failure_class":null, "error_kind":null, "timeout_seconds":57,
            "execution_cpu_timeout_seconds":22, "execution_wall_timeout_seconds":57,
            "argv":[], "guest_argv":[], "env":{}, "cwd":"/home/fixture/work",
            "shell_command":"synthetic retained writer input", "artifact_dir":"synthetic",
            "attempts":[completed["candidate_attempt"],completed["reference_attempt"]],
            "backend_parity":completed["report"]
        });
        let raw = serde_json::to_string(&base).unwrap();
        for (needle, replacement) in [
            ("\"status\":0", "\"status\":7,\"status\":0"),
            ("\"status\":0", "\"status\":0,\"status\":0"),
            ("\"compared\":3", "\"compared\":0,\"compared\":3"),
            ("\"compared\":3", "\"compared\":3,\"compared\":3"),
        ] {
            assert!(raw.contains(needle), "duplicate control omitted {needle}");
            let duplicate = raw.replacen(needle, replacement, 1);
            assert_eq!(serde_json::from_str::<Value>(&duplicate).unwrap(), base);
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("input");
            fs::create_dir(&results).unwrap();
            fs::write(results.join("results.jsonl"), format!("{duplicate}\n")).unwrap();
            let error = retain_v10(parent.path(), &results, &plan).unwrap_err();
            assert!(error.contains("duplicate field"), "{error}");
            assert!(!parent.path().join("ignored/validate/artifacts").exists());
        }
        for case in ["matched", "cross-diverged", "reference-diverged", "reference-no-report", "pass-without-report", "missing-digest"] {
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("input");
            fs::create_dir(&results).unwrap();
            let mut row = base.clone();
            match case {
                "matched" => {},
                "cross-diverged" => {
                    row["outcome"] = Value::String("FAIL".into());
                    row["result"] = Value::String("parity-failure".into());
                    row["failure_class"] = Value::String("product_failure".into());
                    row["reason"] = Value::String("synthetic cross divergence".into());
                    row["backend_parity"]["verdict"] = Value::String("diverged".into());
                    row["backend_parity"]["comparison"]["verdict"] = Value::String("diverged".into());
                    row["backend_parity"]["comparison"]["first_divergent_record"] = Value::from(2);
                },
                "reference-diverged" => {
                    let mut report: VerificationReport = serde_json::from_str(
                        row["attempts"][1]["verification_report"].as_str().unwrap()).unwrap();
                    report.verdict = VerificationVerdict::Diverged;
                    report.verified = false;
                    report.bitwise_parity = false;
                    report.first_divergent_record = Some(1);
                    let raw = serde_json::to_string(&report).unwrap();
                    row["attempts"][1]["verification_report_sha256"] = Value::String(hex_digest(raw.as_bytes()));
                    row["attempts"][1]["verification_report"] = Value::String(raw);
                    row["attempts"][1]["first_divergent_record"] = Value::from(1);
                    row["attempts"][1]["outcome"] = Value::String("FAIL".into());
                    row["attempts"][1]["status"] = Value::from(1);
                    row["attempts"][1]["reason"] = Value::String("synthetic reference divergence".into());
                },
                "reference-no-report" | "pass-without-report" => {
                    row["attempts"][1]["verification_report"] = Value::Null;
                    row["attempts"][1]["verification_report_sha256"] = Value::Null;
                    if case == "reference-no-report" {
                        row["attempts"][1]["outcome"] = Value::String("ERROR".into());
                        row["attempts"][1]["status"] = Value::from(7);
                        row["attempts"][1]["error_kind"] = Value::String("incomplete-verification-evidence".into());
                    }
                },
                "missing-digest" => row["attempts"][1]["verification_report_sha256"] = Value::Null,
                _ => unreachable!(),
            }
            if matches!(case, "reference-diverged" | "reference-no-report" | "pass-without-report" | "missing-digest") {
                row["backend_parity"] = Value::Null;
                row["outcome"] = Value::String("ERROR".into());
                row["result"] = Value::Null;
                row["failure_class"] = Value::String("no_result".into());
                row["error_kind"] = Value::String("incomplete-parity-evidence".into());
                row["reason"] = Value::String("synthetic reference did not provide a matching strict comparison".into());
            }
            fs::write(results.join("results.jsonl"), format!("{}\n", serde_json::to_string(&row).unwrap())).unwrap();
            let result = retain_v10(parent.path(), &results, &plan);
            if matches!(case, "pass-without-report" | "missing-digest") {
                assert!(result.is_err(), "{case} was admitted");
                continue;
            }
            let result = result.unwrap_or_else(|error| panic!("{case}: {error}"));
            assert_eq!(result.schema_version, 10);
            let cell = &result.evidence["cells"][0];
            assert_eq!(cell["cell_verdict"]["state"], "compared-and-matched", "{case}");
            let attempt = &cell["backend_parity"]["attempts"][0];
            assert_eq!(attempt["candidate"]["state"], "compared-and-matched");
            match case {
                "matched" => assert_eq!(attempt["cross"]["state"], "matched"),
                "cross-diverged" => assert_eq!(attempt["cross"]["state"], "diverged"),
                "reference-diverged" => {
                    assert_eq!(attempt["reference"]["state"], "compared-and-diverged");
                    assert_eq!(attempt["cross"]["state"], "unavailable-with-reason");
                },
                "reference-no-report" => {
                    assert_eq!(attempt["reference"]["state"], "unavailable-with-reason");
                    assert!(attempt["reference_verification_report_sha256"].is_null());
                },
                _ => unreachable!(),
            }
            let artifact = result.evidence["artifact"]["path"].as_str().unwrap();
            let bytes = fs::read(parent.path().join(artifact)).unwrap();
            assert_eq!(result.evidence["artifact"]["sha256"], hex_digest(&bytes));
            assert!(String::from_utf8(bytes).unwrap().contains("/home/fixture/"));
            assert!(!serde_json::to_string(&result.evidence).unwrap().contains("/home/fixture/"));
        }
    }

    #[test]
    fn duplicate_report_fields_cannot_become_compared_cell_evidence() {
        let row = result_row("fixture", "1515151515151515151515151515151515151515");
        assert!(matches!(
            cell_verdict(&row).unwrap(),
            CellVerdict::ComparedAndMatched { .. }
        ));
        let raw = row["attempts"][0]["verification_report"].as_str().unwrap();
        for mutation in [
            "unequal",
            "bad-digest",
            "two-dispositions",
            "wrong-disposition",
        ] {
            let mut report: Value = serde_json::from_str(raw).unwrap();
            match mutation {
                "unequal" => {
                    report["compared_outputs"]["right"]["stdout_sha256"] = "c".repeat(64).into()
                }
                "bad-digest" => {
                    for side in ["left", "right"] {
                        report["compared_outputs"][side]["stdout_sha256"] = "invalid".into();
                    }
                }
                "two-dispositions" => {
                    for side in ["left", "right"] {
                        report["compared_outputs"][side]["signal"] = 9.into();
                    }
                }
                "wrong-disposition" => report["guest_exit_code"] = 7.into(),
                _ => unreachable!(),
            }
            let mut bad = row.clone();
            bad["attempts"][0] = attempt(&serde_json::to_string(&report).unwrap());
            assert!(
                matches!(
                    cell_verdict(&bad).unwrap(),
                    CellVerdict::UnavailableWithReason { .. }
                ),
                "{mutation}"
            );
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("results");
            write_result(&results, &bad);
            let retained = retain(
                parent.path(),
                &results,
                string(&bad, "hermit_sha").unwrap(),
                &expected(&bad),
            )
            .unwrap();
            assert_eq!(
                retained.evidence["cells"][0]["cell_verdict"]["state"], "unavailable-with-reason",
                "{mutation}"
            );
        }
        for (needle, replacement) in [
            (r#""verified":true"#, r#""verified":false,"verified":true"#),
            (r#""verified":true"#, r#""verified":true,"verified":true"#),
            (
                r#""no_result_reason":null"#,
                r#""no_result_reason":{"kind":"not_run"},"no_result_reason":null"#,
            ),
            (
                r#""no_result_reason":null"#,
                r#""no_result_reason":null,"no_result_reason":null"#,
            ),
            (r#""left":123"#, r#""left":0,"left":123"#),
            (r#""left":123"#, r#""left":123,"left":123"#),
        ] {
            assert_eq!(raw.matches(needle).count(), 1);
            let duplicated = raw.replacen(needle, replacement, 1);
            let mut bad = row.clone();
            bad["attempts"][0] = attempt(&duplicated);
            match cell_verdict(&bad).unwrap() {
                CellVerdict::UnavailableWithReason { reason, .. } => {
                    assert!(reason.contains("duplicate field"), "{reason}");
                }
                other => panic!("duplicate report produced compared evidence: {other:?}"),
            }
        }
    }

    #[test]
    fn current_timeout_policy_is_required_without_breaking_legacy_raw_reads() {
        let commit = "1515151515151515151515151515151515151515";
        for (label, remove, replacement, expected_error) in [
            (
                "missing",
                Some("execution_cpu_timeout_seconds"),
                None,
                "omitted execution_cpu_timeout_seconds",
            ),
            (
                "half-present",
                Some("execution_wall_timeout_seconds"),
                None,
                "omitted execution_wall_timeout_seconds",
            ),
            (
                "wrong-value",
                None,
                Some(("execution_wall_timeout_seconds", 56_u64)),
                "timeout policy disagrees",
            ),
        ] {
            let root = fixture_root();
            let results = root.join("results");
            let mut row = result_row(&format!("validate-{label}"), commit);
            let object = row.as_object_mut().unwrap();
            if let Some(field) = remove {
                object.remove(field);
            }
            if let Some((field, value)) = replacement {
                object.insert(field.into(), Value::from(value));
            }
            write_result(&results, &row);
            assert_eq!(
                all_result_rows(&results).unwrap().len(),
                1,
                "legacy raw-row reading must retain additive-field compatibility"
            );
            let error = retain(&root, &results, commit, &expected(&row))
                .expect_err("a malformed current timeout policy reached the ledger");
            assert!(error.contains(expected_error), "{label}: {error}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    fn expected(row: &Value) -> Vec<Value> {
        vec![identity_value(&identity(row).unwrap()).unwrap()]
    }

    fn write_result(root: &Path, row: &Value) {
        let directory = root.join("bucket");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("results.jsonl"),
            format!("{}\n", serde_json::to_string(row).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn coverage_separates_selected_from_enabled_but_not_selected() {
        let selected = vec![
            serde_json::json!({
                "lane":"portable", "category":"c-programs", "test":"c-programs/a",
                "mode":"verify", "backend":"ptrace"
            }),
            serde_json::json!({
                "lane":"portable", "category":"c-programs", "test":"c-programs/custom",
                "mode":"custom", "backend":"ptrace"
            }),
        ];
        let cells = serde_json::json!({"cells":[
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/a",
                "mode":"verify", "backend":"ptrace", "enabled":true
            },
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/b",
                "mode":"verify", "backend":"ptrace", "enabled":true,
                "status":"red", "measurement":"measured-and-failed",
                "reason":"excluded after the recorded observations",
                "observations":[{"results":["pass", "fail", "fail"]}]
            },
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/custom",
                "mode":"custom", "backend":"ptrace", "enabled":false
            }
        ]});
        let registration = serde_json::json!({
            "schema":1,
            "present":["covered", "unknown"],
            "ci_registered":["covered"],
            "reason_recorded":[],
            "none_recorded":["unknown"],
            "undeclared":[]
        });
        let planned_nodes = BTreeSet::from([
            "check.example".to_string(),
            "test.example".to_string(),
        ]);
        let planned_test_nodes = BTreeSet::from(["test.example".to_string()]);
        let test_node_coverage = serde_json::json!({
            "planned_test_nodes": 1,
            "executed_test_nodes": 1,
            "zero_executed_nodes": [],
            "absent_nodes": [],
        });
        let scope = coverage_document(
            "full",
            "full",
            &planned_nodes,
            &planned_test_nodes,
            &test_node_coverage,
            &selected,
            &cells,
            &registration,
        )
        .unwrap();
        assert_eq!(scope["plan"]["name"], "full");
        assert_eq!(scope["plan"]["selection_mode"], "full");
        assert_eq!(scope["plan"]["outer_node_count"], 2);
        assert_eq!(scope["plan"]["outer_nodes"][0], "check.example");
        assert_eq!(scope["plan"]["outer_nodes"][1], "test.example");
        assert_eq!(scope["test_nodes"]["planned"][0], "test.example");
        assert_eq!(scope["test_nodes"]["coverage"], test_node_coverage);
        assert_eq!(scope["e2e"]["selected_count"], 2);
        assert_eq!(scope["e2e"]["enabled_count"], 2);
        assert_eq!(scope["e2e"]["selected_and_enabled_count"], 1);
        assert_eq!(scope["e2e"]["enabled_not_selected_count"], 1);
        assert_eq!(scope["e2e"]["selected_not_enabled_count"], 1);
        assert_eq!(scope["e2e"]["enabled_not_selected"][0]["observed_pass_count"], 1);
        assert_eq!(scope["e2e"]["enabled_not_selected"][0]["observed_fail_count"], 2);
        assert_eq!(
            scope["e2e"]["enabled_not_selected"][0]["reason"],
            "excluded after the recorded observations"
        );
        assert_eq!(scope["integration_test_binaries"]["ci_registered"][0], "covered");
        assert_eq!(scope["integration_test_binaries"]["none_recorded"][0], "unknown");
    }

    fn append_result_row(root: &Path, row: &Value) {
        use std::io::Write;

        let directory = root.join("bucket");
        fs::create_dir_all(&directory).unwrap();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("results.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(row).unwrap()).unwrap();
    }

    #[test]
    fn retains_one_closed_schema7_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1111111111111111111111111111111111111111";
        let row = result_row("validate-one", commit);
        write_result(&results, &row);
        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        assert_eq!(retained.schema_version, 7);
        assert_eq!(retained.run_id, "validate-one");
        assert_eq!(retained.evidence["selected_count"], 1);
        assert_eq!(retained.evidence["recorded_count"], 1);
        assert_eq!(
            retained.evidence["population_sha256"],
            "6134692d215e0bd6a4da5514206e3670a377cf32dae1b507453560e62cb41557"
        );
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        let expected_comparison: Value =
            serde_json::from_str::<Value>(&report("matched", "info")).unwrap()["comparison"]
                .clone();
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["comparison"],
            expected_comparison
        );
        let artifact = root.join(retained.evidence["artifact"]["path"].as_str().unwrap());
        let bytes = fs::read(&artifact).unwrap();
        assert_eq!(hex_digest(&bytes), retained.evidence["artifact"]["sha256"]);
        assert!(bytes.ends_with(b"\n"));
        let artifact_row = serde_json::json!({
            "run_id": "validate-one",
            "hermit_sha": commit,
            "source_tree_dirty": false,
            "lane": "portable",
            "category": "c-programs",
            "test": "uname",
            "mode": "verify",
            "backend": "ptrace",
            "cell_verdict": {
                "state": "compared-and-matched",
                "comparison_tier": "canonical-bitwise",
                "comparison": expected_comparison,
                "bitwise_parity": true,
                "compared_log_messages": {"left": 123, "right": 123}
            }
        });
        let mut expected_bytes = serde_json::to_vec(&artifact_row).unwrap();
        expected_bytes.push(b'\n');
        assert_eq!(bytes, expected_bytes, "the shared type must preserve artifact bytes");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_refuses_a_missing_comparison_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1212121212121212121212121212121212121212";
        let mut row = result_row("validate-missing-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report["comparison"]
            .as_object_mut()
            .unwrap()
            .remove("virtualize_time");
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(retained.schema_version, 7);
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict.get("comparison").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_binds_virtual_time_to_the_comparison_mode() {
        for (mode, virtualize_time, expected_state) in [
            ("replay", false, "compared-and-matched"),
            ("verify", true, "compared-and-matched"),
            ("chaos", true, "compared-and-matched"),
            ("replay", true, "unavailable-with-reason"),
            ("verify", false, "unavailable-with-reason"),
            ("chaos", false, "unavailable-with-reason"),
        ] {
            let root = fixture_root();
            let results = root.join("results");
            let commit = "1616161616161616161616161616161616161616";
            let mut row = result_row(&format!("validate-{mode}-{virtualize_time}"), commit);
            row["mode"] = Value::String(mode.into());
            let report = report_with_virtualize_time("matched", "info", virtualize_time);
            row["attempts"][0] = attempt(&report);
            write_result(&results, &row);

            let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
            let verdict = &retained.evidence["cells"][0]["cell_verdict"];
            assert_eq!(
                verdict["state"], expected_state,
                "mode={mode} virtualize_time={virtualize_time}"
            );
            if expected_state == "compared-and-matched" {
                assert_eq!(verdict["comparison"]["virtualize_time"], virtualize_time);
            } else {
                assert!(verdict.get("comparison").is_none());
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn schema7_refuses_unknown_or_missing_mode_before_publishing_an_artifact() {
        for (case, mode) in [
            ("unknown", Some(Value::String("future-mode".into()))),
            ("missing", None),
        ] {
            let root = fixture_root();
            let results = root.join("results");
            let commit = "1717171717171717171717171717171717171717";
            let run_id = format!("validate-{case}-mode");
            let mut row = result_row(&run_id, commit);
            match mode {
                Some(mode) => row["mode"] = mode,
                None => {
                    row.as_object_mut().unwrap().remove("mode");
                }
            }
            write_result(&results, &row);
            let expected = if case == "missing" {
                vec![serde_json::json!({
                    "lane": "portable",
                    "category": "c-programs",
                    "test": "uname",
                    "mode": "verify",
                    "backend": "ptrace"
                })]
            } else {
                expected(&row)
            };

            let error = retain(&root, &results, commit, &expected).unwrap_err();
            if case == "unknown" {
                assert!(error.contains("unsupported cell mode `future-mode`"), "{error}");
            } else {
                assert!(error.contains("no nonempty mode"), "{error}");
            }
            assert!(
                !root
                    .join("ignored/validate/artifacts")
                    .join(&run_id)
                    .join("cell-results.jsonl")
                    .exists(),
                "{case} mode published a retained artifact"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn schema7_names_a_missing_current_verification_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1515151515151515151515151515151515151515";
        let mut row = result_row("validate-missing-report-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report.as_object_mut().unwrap().remove("first_divergent_record");
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict["reason"]
            .as_str()
            .unwrap()
            .contains("missing current producer field `first_divergent_record`"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_refuses_an_unknown_comparison_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1313131313131313131313131313131313131313";
        let mut row = result_row("validate-unknown-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report["comparison"]["future_comparison_field"] = Value::Bool(true);
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(retained.schema_version, 7);
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict.get("comparison").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_refuses_an_unknown_compared_log_messages_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1414141414141414141414141414141414141414";
        let mut row = result_row("validate-unknown-count-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report["compared_log_messages"]["future_count_field"] = Value::from(123);
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(retained.schema_version, 7);
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict.get("compared_log_messages").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retains_recovered_retry_without_rejecting_preserved_history() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "abababababababababababababababababababab";
        let mut first = result_row("validate-retry", commit);
        first["attempt"] = Value::from(1);
        first["outcome"] = Value::String("FAIL".into());
        first["reason"] = Value::String("forced first failure".into());
        first["duration_ms"] = Value::from(111);
        first["timeout_seconds"] = Value::from(15);
        first["execution_cpu_timeout_seconds"] = Value::from(10);
        first["execution_wall_timeout_seconds"] = Value::from(15);
        let mut second = result_row("validate-retry", commit);
        second["attempt"] = Value::from(2);
        second["duration_ms"] = Value::from(222);
        second["timeout_seconds"] = Value::from(15);
        second["execution_cpu_timeout_seconds"] = Value::from(10);
        second["execution_wall_timeout_seconds"] = Value::from(15);
        append_result_row(&results, &first);
        append_result_row(&results, &second);

        let retained = retain(&root, &results, commit, &expected(&second)).unwrap();
        assert_eq!(retained.evidence["recorded_count"], 1);
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        let raw = fs::read_to_string(results.join("bucket/results.jsonl")).unwrap();
        let observations = raw
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0]["attempt"], 1);
        assert_eq!(observations[0]["duration_ms"], 111);
        assert_eq!(observations[0]["timeout_seconds"], 15);
        assert_eq!(observations[1]["attempt"], 2);
        assert_eq!(observations[1]["duration_ms"], 222);
        assert_eq!(observations[1]["timeout_seconds"], 15);
        let history_rows = all_result_rows(&results).unwrap();
        assert_eq!(history_rows, observations);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn product_failure_remains_red_when_the_retry_has_an_infrastructure_error() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        let mut first = result_row("validate-product-then-infrastructure", commit);
        first["attempt"] = Value::from(1);
        first["outcome"] = Value::String("FAIL".into());
        let diverged: Value = serde_json::from_str(&report("diverged", "info")).unwrap();
        replace_report(&mut first, &diverged);
        let mut second = result_row("validate-product-then-infrastructure", commit);
        second["attempt"] = Value::from(2);
        second["outcome"] = Value::String("ERROR".into());
        second["reason"] = Value::String("runner timed out before producing evidence".into());
        append_result_row(&results, &first);
        append_result_row(&results, &second);

        let retained = retain(&root, &results, commit, &expected(&first)).unwrap();
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-diverged"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_retry_history_refuses_a_missing_first_attempt_by_name() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "dededededededededededededededededededede";
        let mut row = result_row("validate-missing-first-attempt", commit);
        row["attempt"] = Value::from(2);
        write_result(&results, &row);

        let error = retain(&root, &results, commit, &expected(&row)).unwrap_err();
        assert!(error.contains("attempt 2"), "{error}");
        assert!(error.contains("expected 1"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_mixed_bucket_run_ids() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "2222222222222222222222222222222222222222";
        let first = result_row("validate-one", commit);
        write_result(&results, &first);
        let second = results.join("other");
        write_result(&second, &result_row("validate-two", commit));
        let error = retain(&root, &results, commit, &expected(&first)).unwrap_err();
        assert!(error.contains("mix run_id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn top_level_failure_cannot_project_a_matched_attempt_as_clean() {
        let mut row = result_row("failed-row", "3333333333333333333333333333333333333333");
        row["outcome"] = Value::String("FAIL".into());
        row["reason"] = Value::String("SaBRe interception path was incomplete".into());
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::UnavailableWithReason { .. }));
    }
    #[test]
    fn unavailable_cell_prefers_exact_attempt_then_row_reason() {
        let mut row = result_row("exact-reason", "3434343434343434343434343434343434343434");
        row["outcome"] = Value::String("ERROR".into());
        row["reason"] = Value::String("row fallback".into());
        row["attempts"] = serde_json::json!([{
            "reason": "KVM guest execution failed: guest exception vector 13"
        }]);

        let exact = match cell_verdict(&row).unwrap() {
            CellVerdict::UnavailableWithReason { reason, .. } => reason,
            other => panic!("untyped attempt became {other:?}"),
        };
        assert_eq!(
            exact,
            "KVM guest execution failed: guest exception vector 13"
        );

        row["attempts"] = serde_json::json!([{}]);
        let fallback = match cell_verdict(&row).unwrap() {
            CellVerdict::UnavailableWithReason { reason, .. } => reason,
            other => panic!("untyped attempt became {other:?}"),
        };
        assert_eq!(fallback, "row fallback");

        row["reason"] = Value::Null;
        let absent = match cell_verdict(&row).unwrap() {
            CellVerdict::UnavailableWithReason { reason, .. } => reason,
            other => panic!("untyped attempt became {other:?}"),
        };
        assert_eq!(absent, "attempt 1 emitted no typed verification report");
    }

    #[test]
    fn infrastructure_error_preserves_its_cause_with_or_without_a_comparison() {
        for retain_comparison in [true, false] {
            let mut row = result_row(
                "infrastructure-row",
                "3434343434343434343434343434343434343434",
            );
            row["outcome"] = Value::String("ERROR".into());
            row["reason"] = Value::String(
                "verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)".into(),
            );
            let mut infrastructure: Value =
                serde_json::from_str(&report("matched", "info")).unwrap();
            infrastructure["verified"] = Value::Bool(false);
            infrastructure["bitwise_parity"] = Value::Bool(false);
            infrastructure["verdict"] = Value::String("infrastructure_error".into());
            infrastructure["infrastructure_error"] =
                serde_json::json!({"kind": "skid_overshoot", "count": 2});
            if !retain_comparison {
                infrastructure["comparison"] = Value::Null;
                infrastructure["compared_log_messages"] = Value::Null;
            }
            replace_report(&mut row, &infrastructure);

            let CellVerdict::UnavailableWithReason { reason, .. } = cell_verdict(&row).unwrap()
            else {
                panic!("infrastructure error became product evidence")
            };
            assert!(reason.contains("2 HERMIT_SKID_OVERSHOOT"), "{reason}");
        }
    }

    #[test]
    fn earlier_divergence_cannot_be_hidden_by_a_final_match() {
        let mut row = result_row("retry-row", "4444444444444444444444444444444444444444");
        let diverged = report("diverged", "info");
        let matched = report("matched", "info");
        row["attempts"] = Value::Array(vec![attempt(&diverged), attempt(&matched)]);
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::ComparedAndDiverged { .. }));
    }

    #[test]
    fn missing_sibling_attempt_cannot_erase_a_divergence_in_either_order() {
        let diverged = report("diverged", "info");
        let missing = serde_json::json!({"outcome": "ERROR"});
        for attempts in [
            vec![attempt(&diverged), missing.clone()],
            vec![missing.clone(), attempt(&diverged)],
        ] {
            let mut row = result_row(
                "missing-sibling",
                "8888888888888888888888888888888888888888",
            );
            row["attempts"] = Value::Array(attempts);
            let verdict = cell_verdict(&row).unwrap();
            assert!(matches!(verdict, CellVerdict::ComparedAndDiverged { .. }));
        }
    }

    #[test]
    fn non_info_sibling_attempt_cannot_erase_a_divergence() {
        let diverged = report("diverged", "info");
        let weaker = report("matched", "deterministic");
        let mut row = result_row("weaker-sibling", "9999999999999999999999999999999999999999");
        row["attempts"] = Value::Array(vec![attempt(&weaker), attempt(&diverged)]);
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::ComparedAndDiverged { .. }));
    }

    #[test]
    fn non_info_scope_never_becomes_a_clean_leg() {
        let mut row = result_row("scope-row", "5555555555555555555555555555555555555555");
        let deterministic = report("matched", "deterministic");
        row["attempts"] = Value::Array(vec![attempt(&deterministic)]);
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::UnavailableWithReason { .. }));
    }

    #[test]
    fn mismatched_report_hash_refuses_the_receipt() {
        let mut row = result_row("hash-row", "7777777777777777777777777777777777777777");
        row["attempts"][0]["verification_report_sha256"] = Value::String("0".repeat(64));
        let error = cell_verdict(&row).unwrap_err();
        assert!(error.contains("verification_report_sha256 mismatch"));
    }

    #[test]
    fn missing_planned_cell_refuses_the_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "6666666666666666666666666666666666666666";
        let row = result_row("missing-row", commit);
        write_result(&results, &row);
        let mut plan = expected(&row);
        let mut missing = plan[0].clone();
        missing["test"] = Value::String("c-programs/missing".into());
        plan.push(missing);
        let error = retain(&root, &results, commit, &plan).unwrap_err();
        assert!(error.contains("1 missing, 0 extra"));
        fs::remove_dir_all(root).unwrap();
    }
}
