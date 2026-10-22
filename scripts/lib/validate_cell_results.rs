// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the per-cell result population carried by one full validate run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use hermit_manifest_plan::ledger::CellBindingContract;
use hermit_manifest_plan::ledger::CellIdentity;
use hermit_manifest_plan::ledger::CellResult as LedgerCellResult;
use hermit_manifest_plan::ledger::CellResultsArtifact;
use hermit_manifest_plan::ledger::CellResultsEvidence;
use hermit_manifest_plan::ledger::CellVerdict;
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

/// An ordinary read and its optional custody evidence. Authority failures never
/// replace the bytes used by the existing ordinary and schema-10 parsers.
pub struct CapturedResults {
    root: PathBuf,
    inputs: Result<BTreeMap<PathBuf, Vec<u8>>, String>,
    custody: Result<Vec<HeldInput>, String>,
}

/// Hold a named inode without accepting symlinks, foreign ownership or file
/// hard links. Directories are held as well as files so renaming an ancestor
/// cannot silently substitute a different result population.
pub struct HeldInput {
    path: PathBuf,
    file: fs::File,
    identity: (u64, u64, u32, u32),
}

impl HeldInput {
    pub fn open(path: &Path, directory: bool, private: bool) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
            )
            .open(path)
            .map_err(|error| format!("cannot hold {}: {error}", path.display()))?;
        Self::from_file(path, file, directory, private)
    }

    fn from_file(
        path: &Path,
        file: fs::File,
        directory: bool,
        private: bool,
    ) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.is_dir() != directory
            || (!directory && (!metadata.is_file() || metadata.nlink() != 1))
            || (private && metadata.mode() & 0o077 != 0)
        {
            return Err(format!(
                "{} is not an owned {} inode",
                path.display(),
                if directory {
                    "directory"
                } else {
                    "single-link regular file"
                }
            ));
        }
        let held = Self {
            path: path.into(),
            file,
            identity: (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.uid(),
            ),
        };
        held.recheck()?;
        Ok(held)
    }

    pub fn create_directory(path: &Path, private: bool) -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut builder = fs::DirBuilder::new();
        if private {
            builder.mode(0o700);
        }
        builder.create(path).map_err(|error| {
            format!(
                "fresh result authority requires create-new {}: {error}",
                path.display()
            )
        })?;
        Self::open(path, true, private)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn recheck(&self) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        for metadata in [
            self.file.metadata().map_err(|error| error.to_string())?,
            fs::symlink_metadata(&self.path)
                .map_err(|error| format!("cannot recheck {}: {error}", self.path.display()))?,
        ] {
            if (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.uid(),
            ) != self.identity
                || (!metadata.is_dir() && metadata.nlink() != 1)
            {
                return Err(format!(
                    "held/named identity changed: {}",
                    self.path.display()
                ));
            }
        }
        Ok(())
    }

    pub fn read(&self) -> Result<Vec<u8>, String> {
        use std::io::Read;
        use std::os::unix::fs::FileExt;
        self.recheck()?;
        let mut bytes = Vec::new();
        let mut offset = 0;
        let mut buffer = [0; 8192];
        loop {
            let count = self
                .file
                .read_at(&mut buffer, offset)
                .map_err(|error| format!("cannot read held {}: {error}", self.path.display()))?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            offset += count as u64;
        }
        // Reopen the name independently. The bytes as well as the identity
        // must agree; same-size in-place edits are not an identity change.
        let named = Self::open(&self.path, false, false)?;
        let mut named_bytes = Vec::new();
        (&named.file)
            .read_to_end(&mut named_bytes)
            .map_err(|error| error.to_string())?;
        if named.identity != self.identity || named_bytes != bytes {
            return Err(format!("held/named bytes changed: {}", self.path.display()));
        }
        self.recheck()?;
        Ok(bytes)
    }
}

fn collect_snapshot_paths(
    path: &Path,
    files: &mut Vec<PathBuf>,
    custody: &mut Result<Vec<HeldInput>, String>,
) -> Result<(), String> {
    keep_custody(custody, HeldInput::open(path, true, false));
    for entry in fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read per-cell result root {}: {error}",
            path.display()
        )
    })? {
        let entry = entry.map_err(|error| format!("cannot read per-cell result entry: {error}"))?;
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot classify {}: {error}", entry.path().display()))?;
        if kind.is_dir() {
            collect_snapshot_paths(&entry.path(), files, custody)?;
        } else if kind.is_file() && entry.file_name() == "results.jsonl" {
            files.push(entry.path());
        } else if kind.is_symlink() && custody.is_ok() {
            // The legacy reader ignores links. That behavior remains ordinary
            // evidence, but cannot attest complete rooted input membership.
            *custody = Err(format!(
                "raw result authority encountered a link: {}",
                entry.path().display()
            ));
        }
    }
    Ok(())
}

fn keep_custody(custody: &mut Result<Vec<HeldInput>, String>, input: Result<HeldInput, String>) {
    if let Ok(held) = custody {
        match input {
            Ok(input) => held.push(input),
            Err(error) => *custody = Err(error),
        }
    }
}

impl CapturedResults {
    pub fn capture(root: &Path) -> Self {
        use std::io::Read;
        let mut custody = Ok(Vec::new());
        let inputs = (|| {
            let mut files = Vec::new();
            collect_snapshot_paths(root, &mut files, &mut custody)?;
            files.sort();
            let mut inputs = BTreeMap::new();
            for path in files {
                // Preserve ordinary open/read semantics. A separate held
                // descriptor may refuse authority without suppressing this read.
                let mut file = fs::File::open(&path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let held = file
                    .try_clone()
                    .map_err(|error| error.to_string())
                    .and_then(|file| HeldInput::from_file(&path, file, false, false));
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                keep_custody(&mut custody, held);
                inputs.insert(
                    path.strip_prefix(root)
                        .map_err(|error| error.to_string())?
                        .into(),
                    bytes,
                );
            }
            Ok(inputs)
        })();
        Self {
            root: root.into(),
            inputs,
            custody,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rows(&self, schema10: bool) -> Result<Vec<(PathBuf, usize, Value)>, String> {
        let mut rows = Vec::new();
        for (relative, bytes) in self.inputs.as_ref().map_err(Clone::clone)? {
            let file = self.root.join(relative);
            let text = std::str::from_utf8(bytes)
                .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
            for (number, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let parsed = if schema10 {
                    hermit_manifest_plan::ledger::read_schema10_source_result(line.as_bytes())
                } else {
                    serde_json::from_str(line).map_err(|error| error.to_string())
                };
                rows.push((
                    file.clone(),
                    number + 1,
                    parsed.map_err(|error| {
                        format!(
                            "{}:{} malformed result row: {error}",
                            file.display(),
                            number + 1
                        )
                    })?,
                ));
            }
        }
        Ok(rows)
    }

    pub fn all_rows(&self) -> Result<Vec<Value>, String> {
        self.rows(false)
            .map(|rows| rows.into_iter().map(|(_, _, row)| row).collect())
    }

    pub fn census(
        &self,
        run_id: &str,
        commit: &str,
        expected_files: &BTreeSet<PathBuf>,
    ) -> Result<hermit_manifest_plan::ledger::RawResultInputCensusV1, String> {
        let inputs = self.inputs.as_ref().map_err(Clone::clone)?;
        if inputs.keys().cloned().collect::<BTreeSet<_>>() != *expected_files {
            return Err("raw input membership differs from completed harness publishers".into());
        }
        let held = self.custody.as_ref().map_err(Clone::clone)?;
        for input in held {
            input.recheck()?;
        }
        let mut current_files = Vec::new();
        let mut current_custody = Ok(Vec::new());
        collect_snapshot_paths(&self.root, &mut current_files, &mut current_custody)?;
        current_custody?;
        let current_files = current_files
            .iter()
            .map(|path| {
                path.strip_prefix(&self.root)
                    .map(Path::to_path_buf)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        if current_files != *expected_files {
            return Err("raw input membership changed before publication".into());
        }
        for input in held {
            if let Ok(relative) = input.path.strip_prefix(&self.root) {
                if let Some(bytes) = inputs.get(relative) {
                    if input.read()? != *bytes {
                        return Err(format!(
                            "raw input bytes changed before publication: {}",
                            relative.display()
                        ));
                    }
                }
            }
            input.recheck()?;
        }
        let inputs = inputs
            .iter()
            .map(|(path, bytes)| {
                path.to_str()
                    .map(|path| (path.to_owned(), bytes.clone()))
                    .ok_or_else(|| "raw result path is not UTF-8".to_owned())
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        hermit_manifest_plan::ledger::RawResultInputCensusV1::from_inputs(run_id, commit, &inputs)
    }

    /// Every retried cell in the captured bytes. Call it only after
    /// [`Self::census`] succeeded, which proves these bytes are still the
    /// files on disk.
    pub fn framework_retries(
        &self,
    ) -> Result<hermit_manifest_plan::ledger::FrameworkRetriesV1, String> {
        let inputs = self
            .inputs
            .as_ref()
            .map_err(Clone::clone)?
            .iter()
            .map(|(path, bytes)| {
                path.to_str()
                    .map(|path| (path.to_owned(), bytes.clone()))
                    .ok_or_else(|| "raw result path is not UTF-8".to_owned())
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        hermit_manifest_plan::ledger::FrameworkRetriesV1::from_inputs(&inputs)
    }
}

#[cfg(test)]
pub fn all_result_rows(path: &Path) -> Result<Vec<Value>, String> {
    CapturedResults::capture(path).all_rows()
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("per-cell result has no nonempty {key}"))
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

fn cell_verdict(row: &Value) -> Result<CellVerdict, String> {
    hermit_manifest_plan::ledger::cell_verdict_from_source(row)
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

fn manifest_cell_scope(cell: &Value) -> Result<Value, String> {
    let mut scoped = identity_value(&identity(cell)?)?
        .as_object()
        .cloned()
        .ok_or("cell identity was not an object")?;
    for key in [
        "status",
        "measurement",
        "reason",
        "last_tested",
        "observations",
    ] {
        if let Some(value) = cell.get(key) {
            scoped.insert(key.into(), value.clone());
        }
    }
    if cell.get("observations").is_none() {
        // A catalogue is selection data. Missing history is not zero history.
        scoped.insert("history_available".into(), Value::Bool(false));
        scoped.insert("observed_pass_count".into(), Value::Null);
        scoped.insert("observed_fail_count".into(), Value::Null);
        scoped.insert("observed_other_results".into(), Value::Null);
        return Ok(Value::Object(scoped));
    }
    scoped.insert("history_available".into(), Value::Bool(true));
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
    let in_manifest: BTreeMap<_, _> = cells_document
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("ci/compat-envelope/cells.json has no cells array")?
        .iter()
        .map(|cell| {
            let id = identity(cell)?;
            Ok((id, manifest_cell_scope(cell)?))
        })
        .collect::<Result<_, String>>()?;
    let selected_and_in_manifest = selected
        .keys()
        .filter(|key| in_manifest.contains_key(*key))
        .count();
    let in_manifest_not_selected_by_path: Vec<Value> = in_manifest
        .iter()
        .filter(|(key, _)| !selected.contains_key(*key))
        .map(|(_, value)| value.clone())
        .collect();
    let selected_not_in_manifest: Vec<Value> = selected
        .iter()
        .filter(|(key, _)| !in_manifest.contains_key(*key))
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
            "selected_by": plan_name,
            "in_manifest_count": in_manifest.len(),
            "selected_and_in_manifest_count": selected_and_in_manifest,
            "in_manifest_not_selected_by_path_count": in_manifest_not_selected_by_path.len(),
            "selected_not_in_manifest_count": selected_not_in_manifest.len(),
            "selected": selected.into_values().collect::<Vec<_>>(),
            "in_manifest_not_selected_by_path": in_manifest_not_selected_by_path,
            "selected_not_in_manifest": selected_not_in_manifest,
        },
        "integration_test_binaries": registration,
    }))
}

/// Retain the exact test population around a full run, including work outside
/// the selected set. This is reporting, not an exemption: a reader can tell a
/// selected cell from a cell in the manifest that this validate path never selected,
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
            output
                .status
                .code()
                .map_or_else(|| "by signal".into(), |code| code.to_string()),
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
    let artifact_dir = parent
        .join("ignored")
        .join("validate")
        .join("artifacts")
        .join(run_id);
    fs::create_dir_all(&artifact_dir).map_err(|error| {
        format!(
            "cannot create retained coverage directory {}: {error}",
            artifact_dir.display()
        )
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
            "selected_by": e2e["selected_by"],
            "in_manifest_count": e2e["in_manifest_count"],
            "selected_and_in_manifest_count": e2e["selected_and_in_manifest_count"],
            "in_manifest_not_selected_by_path_count": e2e["in_manifest_not_selected_by_path_count"],
            "selected_not_in_manifest_count": e2e["selected_not_in_manifest_count"],
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
    Ok(RetainedCoverageEvidence {
        evidence: Value::Object(evidence),
    })
}

/// Transform all result rows for one validate invocation into the closed
/// schema-7 cell-verdict artifact and summary used by ci-hub.
#[cfg(test)]
pub fn retain(
    parent: &Path,
    result_root: &Path,
    commit: &str,
    expected: &[Value],
) -> Result<RetainedCellResults, String> {
    retain_snapshot(
        parent,
        &CapturedResults::capture(result_root),
        commit,
        expected,
    )
}

pub fn retain_snapshot(
    parent: &Path,
    snapshot: &CapturedResults,
    commit: &str,
    expected: &[Value],
) -> Result<RetainedCellResults, String> {
    let mut run_id: Option<String> = None;
    let mut selected = Vec::new();
    let mut identities = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut attempt_rows: BTreeMap<CellIdentity, Vec<(u64, Value)>> = BTreeMap::new();
    for (file, line_number, row) in snapshot.rows(false)? {
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
                // The schema-7 shape predates the evidence binding and stays
                // UNBOUND. Binding is forward-only: `retain_v10` writes it on
                // the current cumulative evidence, and adding it here would
                // put a new field in an old schema without the artifact-side
                // attempt that makes it independently checkable.
                evidence_binding: None,
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

/// Logical names for the two producer-owned roots in recorded commands.
/// These strings are inert provenance, never paths opened by the proof reader.
/// `/repo` is the scorecard's existing measured-root convention; the committed
/// DAG already names the actual raw-results root as `$E2E_RESULT_ROOT`.
struct CommandProvenanceRoots {
    roots: Vec<(String, &'static str)>,
}

impl CommandProvenanceRoots {
    fn new(measured: &Path, results: &Path) -> Result<Self, String> {
        let text = |path: &Path| -> Result<String, String> {
            let value = path
                .to_str()
                .ok_or("command provenance root is not UTF-8")?;
            if !path.is_absolute() || path.parent().is_none() || value.contains('\0') {
                return Err("command provenance root is not an absolute directory identity".into());
            }
            Ok(value.trim_end_matches('/').into())
        };
        let measured = text(measured)?;
        let results = text(results)?;
        let mut roots = vec![(measured.clone(), "/repo")];
        if results != measured {
            roots.push((results, "$E2E_RESULT_ROOT"));
        }
        roots.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        Ok(Self { roots })
    }

    fn delimiter(character: char) -> bool {
        matches!(character,
            '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{0085}' | '\u{00a0}' |
            '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' |
            '\u{202f}' | '\u{205f}' | '\u{3000}' |
            '=' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | '<' | '>' | '"' | '\'')
    }

    fn matches_at(value: &str, offset: usize, root: &str) -> bool {
        value[offset..].starts_with(root)
            && value[..offset]
                .chars()
                .next_back()
                .is_none_or(Self::delimiter)
            && value[offset + root.len()..]
                .chars()
                .next()
                .is_none_or(|character| character == '/' || Self::delimiter(character))
    }

    fn render(&self, value: &str) -> Result<String, String> {
        let mut rendered = String::with_capacity(value.len());
        let mut offset = 0;
        while offset < value.len() {
            if let Some((physical, logical)) = self
                .roots
                .iter()
                .find(|(root, _)| Self::matches_at(value, offset, root))
            {
                let suffix = &value[offset + physical.len()..];
                let token_end = suffix.find(Self::delimiter).unwrap_or(suffix.len());
                if suffix[..token_end]
                    .split('/')
                    .any(|part| matches!(part, "." | ".."))
                {
                    return Err("recorded command escapes or aliases a provenance root".into());
                }
                rendered.push_str(logical);
                offset += physical.len();
                continue;
            }
            // A literal logical path must not collapse with a different
            // physical command after rendering. A real root with that spelling
            // was handled above, including identical measured/results roots.
            if ["/repo", "$E2E_RESULT_ROOT"]
                .iter()
                .any(|logical| Self::matches_at(value, offset, logical))
            {
                return Err("recorded command contains an ambiguous logical root".into());
            }
            let character = value[offset..].chars().next().expect("offset is in bounds");
            rendered.push(character);
            offset += character.len_utf8();
        }
        Ok(rendered)
    }

    fn render_command(
        &self,
        argv: &mut [String],
        cwd: &mut String,
        env: &mut BTreeMap<String, String>,
    ) -> Result<(), String> {
        for argument in argv {
            *argument = self.render(argument)?;
        }
        *cwd = self.render(cwd)?;
        for value in env.values_mut() {
            *value = self.render(value)?;
        }
        Ok(())
    }

    fn render_cell(
        &self,
        cell: &mut hermit_manifest_plan::ledger::CellArtifactResultV10,
    ) -> Result<(), String> {
        use hermit_manifest_plan::cpu_evidence::CpuAttemptHistory;
        if let Some(history) = &mut cell.cpu_observation_history {
            for attempt in &mut history.attempts {
                if let CpuAttemptHistory::Recorded { observations, .. } = attempt {
                    for invocation in &mut observations.invocations {
                        let command = &mut invocation.command;
                        self.render_command(
                            &mut command.argv,
                            &mut command.cwd,
                            &mut command.env_overrides,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Retain each selected cell's full verdict in the immutable artifact and only
/// derived, path-free summaries in schema 10. The selected population comes
/// from the pre-execution plan.
#[cfg(test)]
pub fn retain_v10(
    parent: &Path,
    result_root: &Path,
    plan: &hermit_manifest_plan::ledger::ConstructedValidationPlanV10,
) -> Result<RetainedCellResults, String> {
    retain_v10_snapshot(parent, parent, &CapturedResults::capture(result_root), plan)
}

pub fn retain_v10_snapshot(
    parent: &Path,
    measured_root: &Path,
    snapshot: &CapturedResults,
    plan: &hermit_manifest_plan::ledger::ConstructedValidationPlanV10,
) -> Result<RetainedCellResults, String> {
    // Deploy only after the published parent readers have been activated.
    retain_v10_with_contract(
        parent,
        measured_root,
        snapshot,
        plan,
        CellBindingContract::SelectedAttemptV1,
    )
}

/// Preserve the exact published legacy wire contract as a historical control.
#[cfg(test)]
pub fn retain_v10_legacy(
    parent: &Path,
    result_root: &Path,
    plan: &hermit_manifest_plan::ledger::ConstructedValidationPlanV10,
) -> Result<RetainedCellResults, String> {
    retain_v10_with_contract(
        parent,
        parent,
        &CapturedResults::capture(result_root),
        plan,
        CellBindingContract::LegacyUnbound,
    )
}

fn retain_v10_with_contract(
    parent: &Path,
    measured_root: &Path,
    snapshot: &CapturedResults,
    plan: &hermit_manifest_plan::ledger::ConstructedValidationPlanV10,
    binding_contract: CellBindingContract,
) -> Result<RetainedCellResults, String> {
    use hermit_manifest_plan::ledger::CellArtifactResultV10;
    use hermit_manifest_plan::ledger::CellResultsEvidenceV10;
    use hermit_manifest_plan::ledger::RetiredBackendParity;
    use hermit_manifest_plan::ledger::RetiredParityPopulation;
    let selected = plan.planned_cells()?;
    let selected_set = selected.iter().cloned().collect::<BTreeSet<_>>();
    let mut rows = BTreeMap::<CellIdentity, Vec<(u64, Value)>>::new();
    let mut seen = BTreeSet::new();
    for (file, line, row) in snapshot.rows(true)? {
        if row.get("schema").and_then(Value::as_u64) != Some(4)
            || string(&row, "hermit_sha")? != plan.hermit_sha
            || string(&row, "run_id")? != plan.run_id
            || row.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
        {
            return Err(format!(
                "{}:{line} is not a clean current result for the retained plan",
                file.display()
            ));
        }
        require_current_timeout_policy(&row)?;
        let id = identity(&row)?;
        let attempt = row
            .get("attempt")
            .and_then(Value::as_u64)
            .ok_or("current cell result omitted attempt")?;
        if !selected_set.contains(&id) || !seen.insert((id.clone(), attempt)) {
            return Err("schema 10 results contain an unselected cell or repeated attempt".into());
        }
        rows.entry(id).or_default().push((attempt, row));
    }
    let mut full_cells = Vec::new();
    let command_roots = CommandProvenanceRoots::new(measured_root, snapshot.root())?;
    for (id, mut rows) in rows {
        rows.sort_by_key(|(attempt, _)| *attempt);
        // The ptrace reference run was removed
        // (https://github.com/rrnewton/hermit/issues/3301); a current row that
        // still carries its report or attempt is refused, never read as an
        // ordinary result.
        if rows.iter().any(|(_, row)| {
            row.get("backend_parity")
                .is_some_and(|value| !value.is_null())
                || row
                    .get("attempts")
                    .and_then(Value::as_array)
                    .is_some_and(|attempts| {
                        attempts.iter().any(|attempt| {
                            attempt.get("index").and_then(Value::as_str) == Some("parity-reference")
                        })
                    })
        }) {
            return Err("selected cell emitted the retired ptrace reference comparison".into());
        }
        let outcome = outcome_after_retries(
            rows.iter()
                .map(|(attempt, row)| Ok((*attempt, string(row, "outcome")?)))
                .collect::<Result<Vec<_>, String>>()?,
        )?;
        // The attempt comes from the SAME row the verdict does. Taking it
        // from anywhere else -- the last attempt, the highest ordinal, the
        // count -- would name a real published event that is not the one
        // this verdict was computed from, which is the failure the binding
        // exists to make impossible.
        let (selected_attempt, row) = rows
            .iter()
            .rev()
            .find(|(_, row)| row.get("outcome").and_then(Value::as_str) == Some(outcome))
            .map(|(attempt, row)| (*attempt, row))
            .ok_or("ordinary cell has no selected terminal result")?;
        let cell_verdict = cell_verdict(row)?;
        let cpu_observation_history =
            hermit_manifest_plan::cpu_evidence::CellCpuHistoryV1::from_source_rows(&rows)?;
        if binding_contract == CellBindingContract::LegacyUnbound
            && cpu_observation_history.is_some()
        {
            return Err("legacy cell artifact cannot retain present CPU observations".into());
        }
        let mut cell = CellArtifactResultV10 {
            lane: id.lane,
            category: id.category,
            test: id.test,
            mode: id.mode,
            backend: id.backend,
            cell_verdict,
            backend_parity: RetiredBackendParity,
            cpu_observation_history,
            selected_attempt: (binding_contract == CellBindingContract::SelectedAttemptV1)
                .then_some(selected_attempt),
        };
        // Both constructors above checked the original raw command equality.
        // Render only typed command provenance before the first artifact hash;
        // raw results, comparison reports and CPU observations stay untouched.
        if binding_contract == CellBindingContract::SelectedAttemptV1 {
            command_roots.render_cell(&mut cell)?;
        }
        full_cells.push(cell);
    }
    let mut bytes = Vec::new();
    let mut cells = Vec::new();
    for cell in &full_cells {
        cells.push(cell.summary_for_contract(binding_contract, &plan.run_id, &plan.hermit_sha)?);
        let mut row = serde_json::to_value(cell).map_err(|error| error.to_string())?;
        let object = row
            .as_object_mut()
            .ok_or("schema 10 full cell is not an object")?;
        object.insert("run_id".into(), Value::String(plan.run_id.clone()));
        object.insert("hermit_sha".into(), Value::String(plan.hermit_sha.clone()));
        object.insert("source_tree_dirty".into(), Value::Bool(false));
        serde_json::to_writer(&mut bytes, &row).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
    }
    let population =
        serde_json::to_vec(&serde_json::to_value(&selected).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let artifact_path = super::validate_artifacts::publish_run_artifact_noclobber(
        parent,
        &plan.run_id,
        "cell-results.jsonl",
        &bytes,
        "retained full cell results",
    )?;
    let evidence = CellResultsEvidenceV10 {
        binding_contract,
        path: plan.path,
        run_id: plan.run_id.clone(),
        hermit_sha: plan.hermit_sha.clone(),
        source_tree_dirty: false,
        selected_count: selected.len() as u64,
        recorded_count: cells.len() as u64,
        population_sha256: hex_digest(&population),
        artifact: CellResultsArtifact {
            path: artifact_path,
            sha256: hex_digest(&bytes),
            row_count: cells.len() as u64,
        },
        selected,
        selected_backend_parity: RetiredParityPopulation,
        cells,
    };
    if binding_contract == CellBindingContract::SelectedAttemptV1 {
        evidence.require_bound_compared_cells()?;
    }
    evidence.verify_cell_artifact_bytes(&bytes)?;
    Ok(RetainedCellResults {
        schema_version: 10,
        run_id: plan.run_id.clone(),
        evidence: serde_json::to_value(evidence).map_err(|error| error.to_string())?,
    })
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
    fn catalogue_history_is_unavailable_not_zero_observations() {
        let cell = serde_json::json!({"lane":"portable", "category":"c-programs", "test":"c-programs/a", "mode":"verify", "backend":"ptrace", "status":"green"});
        let scope = manifest_cell_scope(&cell).unwrap();
        assert_eq!(scope["status"], "green");
        assert_eq!(scope["history_available"], false);
        assert!(scope["observed_pass_count"].is_null());
        assert!(scope["observed_fail_count"].is_null());
        let mut empty_history = cell;
        empty_history["observations"] = serde_json::json!([]);
        let scope = manifest_cell_scope(&empty_history).unwrap();
        assert_eq!(scope["history_available"], true);
        assert_eq!(scope["observed_pass_count"], 0);
        assert_eq!(scope["observed_fail_count"], 0);
    }

    #[test]
    fn ordinary_writer_binds_the_row_it_selected() {
        use hermit_manifest_plan::ledger::ConstructedValidationPlanV10;
        // The ordinary writer has its own selected-row branch. The schema-7
        // retry tests below do not prove that V1 binds the row it selected.
        let ordinary_plan: ConstructedValidationPlanV10 = serde_json::from_str(include_str!(
            "../../tests/fixtures/ledger-schema10/legacy/ordinary-only-plan.json"
        ))
        .unwrap();
        let selected = ordinary_plan.planned_cells().unwrap();
        assert_eq!(selected.len(), 1);
        let id = &selected[0];
        let unavailable = |row: &mut Value| {
            row["outcome"] = Value::String("ERROR".into());
            row["reason"] = Value::String("runner produced no comparison report".into());
            row["attempts"] = serde_json::json!([{
                "verification_report": null, "verification_report_sha256": null
            }]);
        };
        for (case, retry, selected_attempt, state) in [
            (
                "fail-then-error",
                Some("ERROR"),
                1u64,
                "compared-and-diverged",
            ),
            ("fail-then-pass", Some("PASS"), 2u64, "compared-and-matched"),
            ("unavailable", None, 1u64, "unavailable-with-reason"),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("input");
            let mut first = result_row(&ordinary_plan.run_id, &ordinary_plan.hermit_sha);
            first["attempt"] = Value::from(1);
            for (field, value) in [
                ("lane", &id.lane),
                ("category", &id.category),
                ("test", &id.test),
                ("mode", &id.mode),
                ("backend", &id.backend),
            ] {
                first[field] = Value::String(value.clone());
            }
            if retry.is_some() {
                first["outcome"] = Value::String("FAIL".into());
                first["reason"] = Value::String("first comparison diverged".into());
                replace_report(
                    &mut first,
                    &serde_json::from_str(&report("diverged", "info")).unwrap(),
                );
            } else {
                unavailable(&mut first);
            }
            let mut written = vec![first.clone()];
            append_result_row(&results, &first);
            if let Some(outcome) = retry {
                let mut second = first;
                second["attempt"] = Value::from(2);
                if outcome == "PASS" {
                    second["outcome"] = Value::String("PASS".into());
                    second["reason"] = Value::Null;
                    replace_report(
                        &mut second,
                        &serde_json::from_str(&report("matched", "info")).unwrap(),
                    );
                } else {
                    unavailable(&mut second);
                }
                assert_eq!(identity(&second).unwrap(), *id, "{case}");
                append_result_row(&results, &second);
                written.push(second);
            }
            assert_eq!(identity(&written[0]).unwrap(), *id, "{case}");
            assert_eq!(written.len(), if retry.is_some() { 2 } else { 1 }, "{case}");
            let result = retain_v10(parent.path(), &results, &ordinary_plan).unwrap();
            assert_eq!(result.schema_version, 10);
            assert_eq!(result.evidence["binding_contract"], 1);
            assert_eq!(result.evidence["selected_count"], 1);
            assert_eq!(result.evidence["recorded_count"], 1);
            assert_eq!(result.evidence["artifact"]["row_count"], 1);
            assert_eq!(result.evidence["cells"].as_array().unwrap().len(), 1);
            let cell = &result.evidence["cells"][0];
            assert_eq!(cell["cell_verdict"]["state"], state, "{case}");
            assert!(cell["backend_parity"].is_null(), "{case}");
            let evidence: hermit_manifest_plan::ledger::CellResultsEvidenceV10 =
                serde_json::from_value(result.evidence.clone()).unwrap();
            let bindings = evidence.bound_attempts().unwrap();
            if retry.is_some() {
                assert_eq!(bindings.len(), 1, "{case}");
                assert_eq!(cell["selected_attempt"], selected_attempt, "{case}");
                assert_eq!(
                    cell["evidence_binding"]["selected_attempt"], selected_attempt,
                    "{case}"
                );
                let binding: hermit_manifest_plan::ledger::CellEvidenceBinding =
                    serde_json::from_value(cell["evidence_binding"].clone()).unwrap();
                binding
                    .verify_against(&ordinary_plan.run_id, &ordinary_plan.hermit_sha, id)
                    .unwrap();
            } else {
                assert!(bindings.is_empty(), "{case}");
                assert!(cell.get("selected_attempt").is_none(), "{case}");
                assert!(cell.get("evidence_binding").is_none(), "{case}");
            }
            let bytes = fs::read(
                parent
                    .path()
                    .join(result.evidence["artifact"]["path"].as_str().unwrap()),
            )
            .unwrap();
            assert_eq!(result.evidence["artifact"]["sha256"], hex_digest(&bytes));
            evidence.verify_cell_artifact_bytes(&bytes).unwrap();
            let artifact: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(artifact["selected_attempt"], selected_attempt, "{case}");
            assert_eq!(artifact["cell_verdict"]["state"], state, "{case}");
            if retry.is_some() {
                assert_eq!(artifact["cell_verdict"], cell["cell_verdict"], "{case}");
            } else {
                assert_eq!(
                    artifact["cell_verdict"]["reason"],
                    "runner produced no comparison report"
                );
            }
            // A recovered match selects attempt 2, but the failed first row is
            // still retained; a later ERROR must not select over that failure.
            assert_eq!(all_result_rows(&results).unwrap(), written, "{case}");
        }
    }

    /// A current row that still carries the removed ptrace reference run, as
    /// its report or as an attempt, is refused rather than retained as an
    /// ordinary result (https://github.com/rrnewton/hermit/issues/3301).
    #[test]
    fn writer_refuses_a_row_carrying_the_retired_reference_run() {
        use hermit_manifest_plan::ledger::ConstructedValidationPlanV10;
        let plan: ConstructedValidationPlanV10 = serde_json::from_str(include_str!(
            "../../tests/fixtures/ledger-schema10/legacy/ordinary-only-plan.json"
        ))
        .unwrap();
        let id = &plan.planned_cells().unwrap()[0];
        let mut base = result_row(&plan.run_id, &plan.hermit_sha);
        base["attempt"] = Value::from(1);
        for (field, value) in [
            ("lane", &id.lane),
            ("category", &id.category),
            ("test", &id.test),
            ("mode", &id.mode),
            ("backend", &id.backend),
        ] {
            base[field] = Value::String(value.clone());
        }
        let parent = tempfile::tempdir().unwrap();
        let results = parent.path().join("input");
        append_result_row(&results, &base);
        retain_v10(parent.path(), &results, &plan).unwrap();
        let mut report = base.clone();
        report["backend_parity"] = serde_json::json!({"verdict": "matched"});
        let mut reference = base.clone();
        let mut attempt = reference["attempts"][0].clone();
        attempt["index"] = Value::String("parity-reference".into());
        reference["attempts"].as_array_mut().unwrap().push(attempt);
        for (case, row) in [("report", report), ("reference attempt", reference)] {
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("input");
            append_result_row(&results, &row);
            let error = retain_v10(parent.path(), &results, &plan).unwrap_err();
            assert!(
                error.contains("retired ptrace reference"),
                "{case}: {error}"
            );
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
    fn coverage_separates_cells_in_manifest_from_cells_selected_by_path() {
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
                "mode":"verify", "backend":"ptrace"
            },
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/b",
                "mode":"verify", "backend":"ptrace",
                "status":"red", "measurement":"measured-and-failed",
                "reason":"excluded after the recorded observations",
                "observations":[{"results":["pass", "fail", "fail"]}]
            },
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/custom",
                "mode":"custom", "backend":"ptrace", "status":"not-applicable"
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
        let planned_nodes =
            BTreeSet::from(["check.example".to_string(), "test.example".to_string()]);
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
        assert_eq!(scope["e2e"]["selected_by"], "full");
        assert_eq!(scope["e2e"]["in_manifest_count"], 3);
        assert_eq!(scope["e2e"]["selected_and_in_manifest_count"], 2);
        assert_eq!(scope["e2e"]["in_manifest_not_selected_by_path_count"], 1);
        assert_eq!(scope["e2e"]["selected_not_in_manifest_count"], 0);
        assert_eq!(
            scope["e2e"]["in_manifest_not_selected_by_path"][0]["observed_pass_count"],
            1
        );
        assert_eq!(
            scope["e2e"]["in_manifest_not_selected_by_path"][0]["observed_fail_count"],
            2
        );
        assert_eq!(
            scope["e2e"]["in_manifest_not_selected_by_path"][0]["reason"],
            "excluded after the recorded observations"
        );
        assert_eq!(
            scope["integration_test_binaries"]["ci_registered"][0],
            "covered"
        );
        assert_eq!(
            scope["integration_test_binaries"]["none_recorded"][0],
            "unknown"
        );
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
        let expected_comparison: Value = serde_json::from_str::<Value>(&report("matched", "info"))
            .unwrap()["comparison"]
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
        assert_eq!(
            bytes, expected_bytes,
            "the shared type must preserve artifact bytes"
        );
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
                assert!(
                    error.contains("unsupported cell mode `future-mode`"),
                    "{error}"
                );
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
        report
            .as_object_mut()
            .unwrap()
            .remove("first_divergent_record");
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(
            verdict["reason"]
                .as_str()
                .unwrap()
                .contains("missing current producer field `first_divergent_record`")
        );
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
            row["reason"] =
                Value::String("verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)".into());
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
    #[test]
    fn command_provenance_roots_preserve_boundaries_and_refuse_aliases() {
        let roots = CommandProvenanceRoots::new(
            Path::new("/physical/source"),
            Path::new("/physical/source/results"),
        )
        .unwrap();
        for (input, expected) in [
            ("/physical/source/target/hermit", "/repo/target/hermit"),
            (
                "--report=/physical/source/results/a.json",
                "--report=$E2E_RESULT_ROOT/a.json",
            ),
            ("/physical/source/results", "$E2E_RESULT_ROOT"),
            (
                "/physical/source:/physical/source/results/x",
                "/repo:$E2E_RESULT_ROOT/x",
            ),
            ("/physical/source-other/file", "/physical/source-other/file"),
            ("prefix/physical/source/file", "prefix/physical/source/file"),
            ("/unknown/owner/file", "/unknown/owner/file"),
            ("credential=unchanged", "credential=unchanged"),
        ] {
            assert_eq!(roots.render(input).unwrap(), expected, "{input}");
        }
        for input in [
            "/repo/target/hermit",
            "$E2E_RESULT_ROOT/a.json",
            "--report=/repo/a.json",
            "/physical/source/../other",
            "/physical/source/./file",
        ] {
            assert!(roots.render(input).is_err(), "{input}");
        }
        let equal =
            CommandProvenanceRoots::new(Path::new("/physical"), Path::new("/physical")).unwrap();
        assert_eq!(equal.render("/physical/file").unwrap(), "/repo/file");
        let reversed =
            CommandProvenanceRoots::new(Path::new("/physical/source"), Path::new("/physical"))
                .unwrap();
        assert_eq!(reversed.render("/physical/source/x").unwrap(), "/repo/x");
        assert_eq!(
            reversed.render("/physical/result").unwrap(),
            "$E2E_RESULT_ROOT/result"
        );
        let already_physical =
            CommandProvenanceRoots::new(Path::new("/repo"), Path::new("/repo/results")).unwrap();
        assert_eq!(
            already_physical.render("/repo/guest").unwrap(),
            "/repo/guest"
        );
        assert_eq!(
            already_physical.render("/repo/results/x").unwrap(),
            "$E2E_RESULT_ROOT/x"
        );
        assert!(
            CommandProvenanceRoots::new(Path::new("relative"), Path::new("/physical")).is_err()
        );
    }

    #[test]
    fn cpu_projection_renders_bound_commands_without_changing_typed_evidence() {
        use hermit_manifest_plan::ledger::CellResultsEvidenceV10;
        use hermit_manifest_plan::ledger::ConstructedValidationPlanV10;
        let plan: ConstructedValidationPlanV10 = serde_json::from_str(include_str!(
            "../../tests/fixtures/ledger-schema10/legacy/ordinary-only-plan.json"
        ))
        .unwrap();
        let template: Value =
            serde_json::from_str(include_str!("fixtures/schema10-matched-cell.json")).unwrap();
        // The candidate's own strict verification, as an ordinary attempt.
        let candidate = &template["backend_parity"]["attempts"][0]["candidate_attempt"];
        let measured = tempfile::tempdir().unwrap();
        // Supported result roots need not be inside the source or state root.
        let results = tempfile::tempdir().unwrap();
        let mut row = serde_json::json!({
            "schema":4,"run_id":plan.run_id,"hermit_sha":plan.hermit_sha,
            "source_tree_dirty":false,"attempt":1,"test":template["test"],
            "category":template["category"],"lane":template["lane"],"mode":"verify","backend":"kvm",
            "classification":"deterministic","outcome":"PASS","result":"pass",
            "failure_class":null,"error_kind":null,"timeout_seconds":57,
            "execution_cpu_timeout_seconds":22,"execution_wall_timeout_seconds":57,
            "argv":[],"guest_argv":[],"env":{},"cwd":"/synthetic/fixture/work",
            "shell_command":"synthetic retained writer input","artifact_dir":"synthetic",
            "attempts":[candidate]
        });
        for attempt in row["attempts"].as_array_mut().unwrap() {
            for key in ["argv", "guest_argv"] {
                for argument in attempt[key].as_array_mut().unwrap() {
                    *argument = argument
                        .as_str()
                        .unwrap()
                        .replace("/synthetic/fixture", measured.path().to_str().unwrap())
                        .into();
                }
            }
            attempt["cwd"] = measured.path().join("work").to_str().unwrap().into();
            attempt["env"] = serde_json::json!({"E2E_FIXTURE_DIR":results.path().join("fixtures")});
        }
        row["cpu_observations"] = cpu_fixture_envelope(&row);
        append_result_row(results.path(), &row);
        let snapshot = CapturedResults::capture(results.path());
        let parent = tempfile::tempdir().unwrap();
        let retained =
            retain_v10_snapshot(parent.path(), measured.path(), &snapshot, &plan).unwrap();
        let bytes = fs::read(
            parent
                .path()
                .join(retained.evidence["artifact"]["path"].as_str().unwrap()),
        )
        .unwrap();
        let evidence: CellResultsEvidenceV10 = serde_json::from_value(retained.evidence).unwrap();
        evidence.verify_cell_artifact_bytes(&bytes).unwrap();
        let artifact: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(artifact["selected_attempt"], 1);
        assert_eq!(artifact["cell_verdict"]["state"], "compared-and-matched");
        let mut observations =
            artifact["cpu_observation_history"]["attempts"][0]["observations"].clone();
        for (i, invocation) in observations["invocations"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            assert_eq!(invocation["command"]["argv"][0], "/repo/hermit");
            assert_eq!(
                invocation["command"]["env_overrides"]["E2E_FIXTURE_DIR"],
                "$E2E_RESULT_ROOT/fixtures"
            );
            invocation["command"] = row["cpu_observations"]["invocations"][i]["command"].clone();
        }
        assert_eq!(observations, row["cpu_observations"]);

        // Even if mapping could hide a raw mismatch, admission checks it first.
        let mut bad = row.clone();
        bad["cpu_observations"]["invocations"][0]["command"]["argv"][0] = "/repo/hermit".into();
        let bad_results = tempfile::tempdir().unwrap();
        append_result_row(bad_results.path(), &bad);
        let bad_parent = tempfile::tempdir().unwrap();
        assert!(
            retain_v10_snapshot(
                bad_parent.path(),
                measured.path(),
                &CapturedResults::capture(bad_results.path()),
                &plan
            )
            .is_err()
        );
        assert!(
            !bad_parent
                .path()
                .join("ignored/validate/artifacts")
                .exists()
        );

        // A literal alias in both the attempt and its CPU observation cannot
        // collapse with another command.
        bad["attempts"][0]["argv"][0] = "/repo/hermit".into();
        let alias_results = tempfile::tempdir().unwrap();
        append_result_row(alias_results.path(), &bad);
        let alias_parent = tempfile::tempdir().unwrap();
        let error = retain_v10_snapshot(
            alias_parent.path(),
            measured.path(),
            &CapturedResults::capture(alias_results.path()),
            &plan,
        )
        .unwrap_err();
        assert!(error.contains("ambiguous logical root"), "{error}");
        assert!(
            !alias_parent
                .path()
                .join("ignored/validate/artifacts")
                .exists()
        );
    }

    // Synthetic reader controls: these values are not native CPU measurements.
    fn cpu_fixture_envelope(row: &Value) -> Value {
        let invocations: Vec<Value> = row["attempts"].as_array().unwrap().iter().enumerate().map(|(i, attempt)| serde_json::json!({
            "ordinal":i+1,"role":{"kind":"execution","attempt_index":attempt["index"],"backend":if attempt["index"] == "parity-reference" {serde_json::json!("ptrace")} else {row["backend"].clone()}},
            "command":{"argv":attempt["argv"],"cwd":attempt["cwd"],"env_overrides":attempt["env"]},
            "launch":{"state":"spawned","pid":17},"live":{"state":"disabled"},
            "final_wait":{"state":"reaped","source":"wait4","pid":17,"raw_status":0,"at":{"seconds":1,"nanoseconds":0},"cpu":{"state":"measured","user_usec":3,"system_usec":2,"total_usec":5}},
            "termination":"completed_wait4","returned_cpu_charge":{"state":"value","cpu_usec":5,"basis":"final_wait4"}
        })).collect();
        serde_json::json!({"version":1,"binding":{"run_id":row["run_id"],"hermit_sha":row["hermit_sha"],"lane":row["lane"],"category":row["category"],"test":row["test"],"mode":row["mode"],"backend":row["backend"],"outer_attempt":row["attempt"],"run_index":null},"invocations":invocations})
    }

    #[test]
    fn cpu_projection_preserves_every_ordinary_retry_and_refuses_hidden_malformed_rows() {
        use hermit_manifest_plan::ledger::ConstructedValidationPlanV10;
        let plan: ConstructedValidationPlanV10 = serde_json::from_str(include_str!(
            "../../tests/fixtures/ledger-schema10/legacy/ordinary-only-plan.json"
        ))
        .unwrap();
        let id = plan.planned_cells().unwrap().remove(0);
        let mut first = result_row(&plan.run_id, &plan.hermit_sha);
        for (key, value) in [
            ("lane", &id.lane),
            ("category", &id.category),
            ("test", &id.test),
            ("mode", &id.mode),
            ("backend", &id.backend),
        ] {
            first[key] = Value::String(value.clone());
        }
        first["attempt"] = Value::from(1);
        first["outcome"] = Value::String("FAIL".into());
        replace_report(
            &mut first,
            &serde_json::from_str(&report("diverged", "info")).unwrap(),
        );
        let mut second = first.clone();
        second["attempt"] = Value::from(2);
        second["outcome"] = Value::String("PASS".into());
        replace_report(
            &mut second,
            &serde_json::from_str(&report("matched", "info")).unwrap(),
        );
        for row in [&mut first, &mut second] {
            row["attempts"][0]["index"] = Value::String("verify-1".into());
            row["attempts"][0]["argv"] = serde_json::json!(["synthetic-command"]);
            row["attempts"][0]["cwd"] = Value::String("/synthetic".into());
            row["attempts"][0]["env"] = serde_json::json!({});
        }
        let mut old_bytes = None;
        for mask in 0..4 {
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("results");
            let mut rows = [first.clone(), second.clone()];
            for (i, row) in rows.iter_mut().enumerate() {
                if mask & (1 << i) != 0 {
                    row["cpu_observations"] = cpu_fixture_envelope(row);
                }
                append_result_row(&results, row);
            }
            let retained = retain_v10(parent.path(), &results, &plan).unwrap();
            let bytes = fs::read(
                parent
                    .path()
                    .join(retained.evidence["artifact"]["path"].as_str().unwrap()),
            )
            .unwrap();
            let artifact: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(artifact["selected_attempt"], 2);
            assert_eq!(artifact["cell_verdict"]["state"], "compared-and-matched");
            let evidence: hermit_manifest_plan::ledger::CellResultsEvidenceV10 =
                serde_json::from_value(retained.evidence.clone()).unwrap();
            evidence.verify_cell_artifact_bytes(&bytes).unwrap();
            if mask == 0 {
                assert!(artifact.get("cpu_observation_history").is_none());
                old_bytes = Some(bytes);
            } else {
                let history = &artifact["cpu_observation_history"];
                assert_eq!(history["attempts"].as_array().unwrap().len(), 2);
                for i in 0..2 {
                    assert_eq!(history["attempts"][i]["outer_attempt"], i + 1);
                    assert_eq!(
                        history["attempts"][i]["state"],
                        if mask & (1 << i) != 0 {
                            "recorded"
                        } else {
                            "unrecorded"
                        }
                    );
                    if mask & (1 << i) != 0 {
                        assert_eq!(
                            history["attempts"][i]["observations"],
                            rows[i]["cpu_observations"]
                        );
                    }
                }
                let mut without = artifact.clone();
                without
                    .as_object_mut()
                    .unwrap()
                    .remove("cpu_observation_history");
                let mut historical = serde_json::to_vec(&without).unwrap();
                historical.push(b'\n');
                assert_eq!(historical, old_bytes.as_ref().unwrap().as_slice());
            }
        }
        for bad in [Value::Null, serde_json::json!({"version":99}), {
            let mut value = cpu_fixture_envelope(&first);
            value["binding"]["test"] = Value::String("foreign".into());
            value
        }] {
            let parent = tempfile::tempdir().unwrap();
            let results = parent.path().join("results");
            let mut row = first.clone();
            row["cpu_observations"] = bad;
            append_result_row(&results, &row);
            append_result_row(&results, &second);
            assert!(retain_v10(parent.path(), &results, &plan).is_err());
            assert!(
                !parent
                    .path()
                    .join("ignored/validate/artifacts")
                    .join(&plan.run_id)
                    .join("cell-results.jsonl")
                    .exists()
            );
        }
        let parent = tempfile::tempdir().unwrap();
        let results = parent.path().join("results");
        first["cpu_observations"] = cpu_fixture_envelope(&first);
        append_result_row(&results, &first);
        append_result_row(&results, &second);
        assert!(
            retain_v10_legacy(parent.path(), &results, &plan)
                .unwrap_err()
                .contains("CPU observations")
        );
    }

    #[test]
    fn raw_snapshot_keeps_empty_files_original_bytes_and_superseded_attempts() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("raw");
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("bucket")).unwrap();
        fs::write(root.join("empty/results.jsonl"), b"").unwrap();
        let commit = "abababababababababababababababababababab";
        let mut first = result_row("captured-run", commit);
        first["attempt"] = 1.into();
        first["outcome"] = "FAIL".into();
        let mut second = result_row("captured-run", commit);
        second["attempt"] = 2.into();
        let bytes = format!("\n{first}\n \t\n{second}\n").into_bytes();
        fs::write(root.join("bucket/results.jsonl"), &bytes).unwrap();
        let snapshot = CapturedResults::capture(&root);
        let members = ["bucket/results.jsonl", "empty/results.jsonl"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let census = snapshot.census("captured-run", commit, &members).unwrap();
        assert_eq!(census.files.len(), 2);
        assert_eq!(census.files[0].bytes, bytes.len() as u64);
        assert_eq!(census.files[0].sha256, hex_digest(&bytes));
        assert_eq!(
            census.files[0]
                .rows
                .iter()
                .map(|row| (row.line, row.attempt))
                .collect::<Vec<_>>(),
            vec![(2, 1), (4, 2)]
        );
        assert_eq!(census.files[1].bytes, 0);
        assert!(census.files[1].rows.is_empty());
        assert_eq!(snapshot.all_rows().unwrap(), vec![first, second.clone()]);
        let projection =
            retain_snapshot(parent.path(), &snapshot, commit, &expected(&second)).unwrap();
        assert_eq!(projection.evidence["recorded_count"], 1);
        assert_eq!(
            projection.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        // A late authority refusal does not replace the snapshot's ordinary projection.
        fs::write(
            root.join("bucket/results.jsonl"),
            b"malformed late replacement\n",
        )
        .unwrap();
        assert!(snapshot.census("captured-run", commit, &members).is_err());
        assert_eq!(snapshot.all_rows().unwrap().len(), 2);
        assert_eq!(snapshot.all_rows().unwrap()[1], second);
    }

    #[test]
    fn raw_snapshot_final_fence_rejects_membership_inode_and_same_size_changes() {
        for opponent in [
            "same-size",
            "same-bytes-new-inode",
            "append",
            "add-empty",
            "rename-empty",
            "remove-empty",
            "root-replacement",
        ] {
            let parent = tempfile::tempdir().unwrap();
            let root = parent.path().join("raw");
            fs::create_dir_all(root.join("empty")).unwrap();
            fs::write(root.join("empty/results.jsonl"), b"").unwrap();
            let commit = "abababababababababababababababababababab";
            let mut row = result_row("captured-run", commit);
            row["attempt"] = 1.into();
            let bytes = format!("{row}\n").into_bytes();
            let path = root.join("results.jsonl");
            fs::write(&path, &bytes).unwrap();
            let snapshot = CapturedResults::capture(&root);
            let members = ["empty/results.jsonl", "results.jsonl"]
                .into_iter()
                .map(PathBuf::from)
                .collect();
            snapshot.census("captured-run", commit, &members).unwrap();
            match opponent {
                "same-size" => {
                    let changed = String::from_utf8(bytes.clone())
                        .unwrap()
                        .replace("\"PASS\"", "\"FAIL\"");
                    assert_eq!(changed.len(), bytes.len());
                    assert_ne!(changed.as_bytes(), bytes);
                    fs::write(&path, changed).unwrap();
                }
                "same-bytes-new-inode" => {
                    fs::rename(&path, root.join("old-input")).unwrap();
                    fs::write(&path, &bytes).unwrap();
                }
                "append" => fs::write(&path, [bytes.as_slice(), b"\n"].concat()).unwrap(),
                "add-empty" => {
                    fs::create_dir(root.join("new")).unwrap();
                    fs::write(root.join("new/results.jsonl"), b"").unwrap();
                }
                "rename-empty" => fs::rename(root.join("empty"), root.join("renamed")).unwrap(),
                "remove-empty" => fs::remove_file(root.join("empty/results.jsonl")).unwrap(),
                "root-replacement" => {
                    fs::rename(&root, parent.path().join("old-root")).unwrap();
                    fs::create_dir_all(root.join("empty")).unwrap();
                    fs::write(&path, &bytes).unwrap();
                    fs::write(root.join("empty/results.jsonl"), b"").unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                snapshot.census("captured-run", commit, &members).is_err(),
                "{opponent}"
            );
            assert_eq!(snapshot.all_rows().unwrap(), vec![row], "{opponent}");
        }
    }

    #[test]
    fn raw_snapshot_preserves_legacy_parser_and_caches_real_read_failures() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("missing");
        let failed = CapturedResults::capture(&missing);
        fs::create_dir(&missing).unwrap();
        fs::write(
            missing.join("results.jsonl"),
            b"{\"value\":1,\"value\":2}\n",
        )
        .unwrap();
        assert!(
            failed.all_rows().is_err(),
            "later input cannot repair a failed acquisition"
        );
        assert!(
            failed
                .census("run", &"a".repeat(40), &BTreeSet::new())
                .is_err()
        );
        let captured = CapturedResults::capture(&missing);
        assert_eq!(
            captured.all_rows().unwrap(),
            vec![serde_json::json!({"value":2})]
        );
        assert!(captured.rows(true).unwrap_err().contains("duplicate field"));
    }

    #[test]
    fn raw_snapshot_custody_refusal_leaves_ordinary_symlink_root_readable() {
        let parent = tempfile::tempdir().unwrap();
        let real = parent.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("results.jsonl"), b"{\"ordinary\":true}\n").unwrap();
        let alias = parent.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let snapshot = CapturedResults::capture(&alias);
        assert_eq!(
            snapshot.all_rows().unwrap(),
            vec![serde_json::json!({"ordinary":true})]
        );
        assert!(
            snapshot
                .census(
                    "run",
                    &"a".repeat(40),
                    &[PathBuf::from("results.jsonl")].into_iter().collect()
                )
                .is_err()
        );
    }

    #[test]
    fn raw_authority_requires_create_new_root_and_retains_its_identity() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("private");
        let held = HeldInput::create_directory(&path, true).unwrap();
        assert!(HeldInput::create_directory(&path, true).is_err());
        held.recheck().unwrap();
        fs::rename(&path, parent.path().join("old")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(held.recheck().is_err());
    }
}
