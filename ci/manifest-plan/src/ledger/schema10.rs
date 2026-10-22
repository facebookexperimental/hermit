//! Cumulative validation evidence with an independently retained plan and
//! each cell's own ordinary comparison.

use dagrun::io::dag_from_json;
use dagrun::io::dag_to_json;
use dagrun::model::DagConfig;
use dagrun::model::DagManifest;

use super::*;

pub const VALIDATION_EVIDENCE_SCHEMA_VERSION: u32 = 10;

/// Optional producer proof of the original raw input population. This is an
/// independent versioned extension, not a reinterpretation of historical v10
/// reduced cell artifacts. Absence never acquires authority from current files.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawResultInputCensusV1 {
    pub schema: u32,
    pub run_id: String,
    pub hermit_sha: String,
    pub files: Vec<RawResultInputFileV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawResultInputFileV1 {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub rows: Vec<RawResultInputRowV1>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawResultInputRowV1 {
    pub line: u64,
    pub cell: CellIdentity,
    pub attempt: u64,
}

impl RawResultInputCensusV1 {
    /// Freeze exactly the bytes observed by the producer before finalization.
    /// Comparison semantics remain the existing cell/comparison readers' job.
    pub fn from_inputs(
        run_id: &str,
        hermit_sha: &str,
        inputs: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, String> {
        if !nonblank_component(run_id) || !is_lower_hex(hermit_sha, 40) {
            return Err("raw result census requires a run identity and full measured SHA".into());
        }
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for (path, bytes) in inputs {
            if path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains('\0'))
                || path.rsplit('/').next() != Some("results.jsonl")
            {
                return Err("raw result census path is not a relative results.jsonl path".into());
            }
            let text = std::str::from_utf8(bytes)
                .map_err(|error| format!("raw result census input is not UTF-8: {error}"))?;
            let mut rows = Vec::new();
            for (number, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let row = read_schema10_source_result(line.as_bytes())?;
                if row.get("schema").and_then(Value::as_u64) != Some(4)
                    || row.get("run_id").and_then(Value::as_str) != Some(run_id)
                    || row.get("hermit_sha").and_then(Value::as_str) != Some(hermit_sha)
                    || row.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
                {
                    return Err(
                        "raw result census input has a different run or clean source identity"
                            .into(),
                    );
                }
                let field = |name: &str| {
                    row.get(name)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| format!("raw result census input omitted {name}"))
                };
                let cell = CellIdentity {
                    lane: field("lane")?,
                    category: field("category")?,
                    test: field("test")?,
                    mode: field("mode")?,
                    backend: field("backend")?,
                };
                validate_identity(&cell)?;
                let attempt = row
                    .get("attempt")
                    .and_then(Value::as_u64)
                    .filter(|value| *value > 0)
                    .ok_or("raw result census input omitted a positive attempt")?;
                if !seen.insert((cell.clone(), attempt)) {
                    return Err("raw result census repeats a cell attempt".into());
                }
                rows.push(RawResultInputRowV1 {
                    line: number as u64 + 1,
                    cell,
                    attempt,
                });
            }
            files.push(RawResultInputFileV1 {
                path: path.clone(),
                bytes: bytes.len() as u64,
                sha256: hex_digest(bytes),
                rows,
            });
        }
        Ok(Self {
            schema: 1,
            run_id: run_id.into(),
            hermit_sha: hermit_sha.into(),
            files,
        })
    }

    pub fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        if self.schema != 1
            || !matches!(row.schema_version, Some(5..=10))
            || !nonblank_component(&self.run_id)
            || !is_lower_hex(&self.hermit_sha, 40)
            || row.run_id.as_deref() != Some(&self.run_id)
            || row.commit.as_deref() != Some(&self.hermit_sha)
            || row.tree_dirty != Some(false)
        {
            return Err("raw result census has an unsupported version or row identity".into());
        }
        let mut previous: Option<&str> = None;
        let mut seen = BTreeSet::new();
        for file in &self.files {
            if file
                .path
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | "..") || part.contains('\0'))
                || file.path.rsplit('/').next() != Some("results.jsonl")
                || previous.is_some_and(|path| path >= file.path.as_str())
                || !is_lower_hex(&file.sha256, 64)
            {
                return Err("raw result census has an invalid or repeated file identity".into());
            }
            previous = Some(&file.path);
            let mut line = 0;
            for input in &file.rows {
                validate_identity(&input.cell)?;
                if input.line <= line
                    || input.attempt == 0
                    || !seen.insert((&input.cell, input.attempt))
                {
                    return Err("raw result census has an invalid or repeated row identity".into());
                }
                line = input.line;
            }
        }
        Ok(())
    }

    pub fn verify_inputs(
        &self,
        row: &HistoryRow,
        inputs: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), String> {
        self.validate_for_row(row)?;
        let actual = Self::from_inputs(&self.run_id, &self.hermit_sha, inputs)?;
        if *self != actual {
            return Err(
                "current result population or bytes differ from the producer-finalized raw census"
                    .into(),
            );
        }
        Ok(())
    }
}

impl HistoryRow {
    /// Verify finalized producer semantics against the complete retained raw
    /// population and, when required, the original plan/cell/test artifacts.
    /// Returns whether that population has zero rows, only after the full
    /// selected-zero checks. Empty files remain separate census members.
    ///
    /// This does not establish canonical origin, select a ledger row, retain
    /// file custody, or authorize a history proof. The canonical adapter must
    /// acquire the exact row and repeat its query before durable publication.
    pub fn verify_finalized_raw_input_bytes(
        &self,
        measured: &str,
        started_at: &str,
        inputs: &BTreeMap<String, Vec<u8>>,
        artifact_bytes: Option<(&[u8], &[u8], &[u8])>,
    ) -> Result<bool, String> {
        self.run_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or("finalized row omitted its run identity")?;
        if self.commit.as_deref() != Some(measured)
            || self.tree_dirty != Some(false)
            || self.commit_anchored != Some(true)
            || self.started_at.as_deref() != Some(started_at)
            || self
                .finished_at
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            || !matches!(self.result.as_deref(), Some("pass" | "fail" | "no_result"))
        {
            return Err(
                "finalized row does not bind a terminal clean measured run and its stamp".into(),
            );
        }
        let census = self.raw_result_input_census_v1()?.ok_or_else(|| {
            format!(
                "finalized run has no producer-bound raw input census: {}",
                self.extra
                    .get("raw_result_input_census_error")
                    .and_then(Value::as_str)
                    .unwrap_or("historical absence cannot authorize current writeback")
            )
        })?;
        census.verify_inputs(self, inputs)?;
        let zero_cells = census.files.iter().all(|file| file.rows.is_empty());
        if self.schema_version == Some(10) || zero_cells {
            self.constructed_plan_artifact()?
                .ok_or("zero-current proof requires schema 10")?;
            let (plan, cells, tests) = artifact_bytes
                .ok_or("finalized proof omitted required plan/cell/test artifact bytes")?;
            let verified = self
                .verify_schema10_artifact_bytes(plan, cells, tests)?
                .ok_or("zero-current proof did not establish schema 10 evidence")?;
            let cells = &verified.cell_results;
            let recorded = cells
                .cells
                .iter()
                .map(|cell| cell.identity())
                .collect::<BTreeSet<_>>();
            let actual = census
                .files
                .iter()
                .flat_map(|file| &file.rows)
                .map(|row| row.cell.clone())
                .collect::<BTreeSet<_>>();
            if recorded != actual {
                return Err(
                    "current raw cells differ from the verified recorded artifact population"
                        .into(),
                );
            }
            for cell in &cells.cells {
                if let Some(selected_attempt) = cell.selected_attempt {
                    if !census
                        .files
                        .iter()
                        .flat_map(|file| &file.rows)
                        .any(|input| {
                            input.cell == cell.identity() && input.attempt == selected_attempt
                        })
                    {
                        return Err(
                            "verified selected attempt is absent from the raw input census".into(),
                        );
                    }
                }
            }
            // Nonempty partial failures retain their missing planned work.
            // Empty raw input must satisfy every existing selected-zero check.
            if zero_cells
                && (cells.selected_count != 0
                    || cells.recorded_count != 0
                    || cells.artifact.row_count != 0
                    || !cells.selected.is_empty()
                    || !cells.cells.is_empty()
                    || !verified.missing_cells.is_empty()
                    || !verified.missing_test_producers.is_empty()
                    || !verified.full_test_results)
            {
                return Err(
                    "empty current results contradict the finalized selected population".into(),
                );
            }
        }
        Ok(zero_cells)
    }

    /// Preserve absence and the outer extension map's historical serialization.
    /// A present unknown/malformed version refuses, rather than becoming absent.
    pub fn raw_result_input_census_v1(&self) -> Result<Option<RawResultInputCensusV1>, String> {
        let Some(value) = self.extra.value("raw_result_input_census_v1") else {
            return Ok(None);
        };
        if self.extra.get("raw_result_input_census_error").is_some() {
            return Err("raw result census cannot carry both proof and publication error".into());
        }
        let census: RawResultInputCensusV1 = serde_json::from_value(value.into_owned())
            .map_err(|error| format!("invalid raw result census: {error}"))?;
        census.validate_for_row(self)?;
        Ok(Some(census))
    }
}

/// The comparison-linkage contract, separate from the outer evidence schema.
/// Legacy rows remain authenticated observations, never inferred bindings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CellBindingContract {
    #[default]
    LegacyUnbound,
    SelectedAttemptV1,
}

impl CellBindingContract {
    pub fn is_legacy_unbound(&self) -> bool {
        *self == Self::LegacyUnbound
    }
}

impl Serialize for CellBindingContract {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::SelectedAttemptV1 => serializer.serialize_u64(1),
            Self::LegacyUnbound => Err(serde::ser::Error::custom(
                "legacy binding contract must be omitted, not encoded",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for CellBindingContract {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u64::deserialize(deserializer)? {
            1 => Ok(Self::SelectedAttemptV1),
            _ => Err(serde::de::Error::custom("unknown cell binding contract")),
        }
    }
}

/// Read the original harness row before any map conversion can erase duplicate
/// fields. Non-null durations must fit an exact unsigned 64-bit integer before
/// buffering; other numbers retain serde_json's existing representation, which
/// includes legitimate floating-point diversity measurements in ordinary rows.
/// Historical runner and ledger decoders retain their existing behavior.
pub fn read_schema10_source_result(bytes: &[u8]) -> Result<Value, String> {
    struct ExactValue(Value);
    impl<'de> Deserialize<'de> for ExactValue {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = ExactValue;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("JSON with unique object fields and exact integer durations")
                }

                fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                    Ok(ExactValue(Value::Null))
                }

                fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                    Ok(ExactValue(Value::Bool(value)))
                }

                fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                    Ok(ExactValue(Value::from(value)))
                }

                fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                    Ok(ExactValue(Value::from(value)))
                }

                fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                    serde_json::Number::from_f64(value)
                        .map(|number| ExactValue(Value::Number(number)))
                        .ok_or_else(|| E::custom("non-finite JSON number"))
                }

                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(ExactValue(Value::String(value.into())))
                }

                fn visit_seq<A: serde::de::SeqAccess<'de>>(
                    self,
                    mut seq: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut values = Vec::new();
                    while let Some(value) = seq.next_element::<ExactValue>()? {
                        values.push(value.0);
                    }
                    Ok(ExactValue(Value::Array(values)))
                }

                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut values = serde_json::Map::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if values.contains_key(&key) {
                            return Err(serde::de::Error::custom(format!(
                                "duplicate field `{key}` in schema 10 source result"
                            )));
                        }
                        let value = map.next_value::<ExactValue>()?.0;
                        values.insert(key, value);
                    }
                    Ok(ExactValue(Value::Object(values)))
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }
    let value = serde_json::from_slice::<ExactValue>(bytes)
        .map(|value| value.0)
        .map_err(|error| format!("malformed schema 10 source result: {error}"))?;
    let exact_duration = |object: &Value| {
        if object
            .get("duration_ms")
            .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
        {
            Err("schema 10 duration_ms must be an exact unsigned 64-bit integer".to_string())
        } else {
            Ok(())
        }
    };
    exact_duration(&value)?;
    if let Some(attempts) = value.get("attempts").and_then(Value::as_array) {
        for attempt in attempts {
            exact_duration(attempt)?;
        }
    }
    crate::cpu_evidence::validate_cpu_observations_in_source_row(&value)?;
    Ok(value)
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConstructedPlanArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConstructedValidationPlanV10 {
    pub schema: u64,
    pub run_id: String,
    pub hermit_sha: String,
    pub path: ValidatePath,
    pub compatibility_selected: bool,
    pub dag_json: String,
    pub expected_e2e_plan_json: String,
}

fn validate_identity(identity: &CellIdentity) -> Result<(), String> {
    if [
        &identity.lane,
        &identity.category,
        &identity.test,
        &identity.mode,
        &identity.backend,
    ]
    .iter()
    .any(|field| field.is_empty() || field.trim() != field.as_str())
    {
        return Err("schema 10 cell identity is empty or untrimmed".into());
    }
    Ok(())
}

fn exact_identity(cell: &DagManifest) -> Result<CellIdentity, String> {
    let identity = CellIdentity {
        lane: cell.lane.clone(),
        category: cell.category.clone(),
        test: cell
            .test
            .clone()
            .ok_or("constructed plan cell has no exact test")?,
        mode: cell
            .mode
            .clone()
            .ok_or("constructed plan cell has no exact mode")?,
        backend: cell
            .backend
            .clone()
            .ok_or("constructed plan cell has no exact backend")?,
    };
    validate_identity(&identity)?;
    Ok(identity)
}

fn selector_matches(selector: &DagManifest, cell: &CellIdentity) -> bool {
    selector.lane == cell.lane
        && selector.category == cell.category
        && selector
            .test
            .as_ref()
            .is_none_or(|value| value == &cell.test)
        && selector
            .mode
            .as_ref()
            .is_none_or(|value| value == &cell.mode)
        && selector
            .backend
            .as_ref()
            .is_none_or(|value| value == &cell.backend)
}

impl ConstructedValidationPlanV10 {
    pub fn constructed_dag(&self) -> Result<DagConfig, String> {
        if self.schema != 1
            || !nonblank_component(&self.run_id)
            || !is_lower_hex(&self.hermit_sha, 40)
        {
            return Err("schema 10 constructed plan identity is malformed".into());
        }
        let cfg = dag_from_json(&self.dag_json)
            .map_err(|error| format!("invalid schema 10 constructed DAG: {error}"))?;
        if dag_to_json(&cfg) != self.dag_json {
            return Err(
                "schema 10 constructed plan is not the exact canonical selected DAG".into(),
            );
        }
        Ok(cfg)
    }

    /// The selected cells of this plan, from its own result-owning steps.
    ///
    /// A retained plan with a step that asked the harness for a ptrace
    /// reference run (`--parity-reference`) is refused: that run and the
    /// cross-backend evidence it produced were removed by
    /// <https://github.com/rrnewton/hermit/issues/3301>, and parity is now
    /// measured only by the [`crate::parity`] post-pass. No retained ledger
    /// row selected such a step, so the refusal excludes no published row.
    pub fn planned_cells(&self) -> Result<Vec<CellIdentity>, String> {
        let cfg = self.constructed_dag()?;
        let expected =
            crate::validation_dag::expected_cells_from_json(&self.expected_e2e_plan_json)?
                .iter()
                .map(exact_identity)
                .collect::<Result<BTreeSet<_>, _>>()?;
        let mut selected = BTreeSet::new();
        let mut tags = BTreeSet::new();
        for step in &cfg.steps {
            if !tags.insert(step.tag()) {
                return Err("constructed plan repeats a step identity".into());
            }
            if step.cmd.contains("--parity-reference") {
                return Err(format!(
                    "{} requests the retired ptrace reference run; plans that did are excluded (https://github.com/rrnewton/hermit/issues/3301)",
                    step.tag()
                ));
            }
            let mut owned = BTreeSet::new();
            for manifest in step.effective_result_manifests().iter() {
                let identity = exact_identity(manifest)?;
                if !expected.contains(&identity) || !owned.insert(identity.clone()) {
                    return Err(format!(
                        "{} owns an unknown or repeated expected cell",
                        step.tag()
                    ));
                }
                selected.insert(identity);
            }
            if let Some(selector) = &step.manifest {
                let required = expected
                    .iter()
                    .filter(|cell| selector_matches(selector, cell))
                    .filter(|cell| !crate::validation_dag::step_omits_backend(step, &cell.backend))
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if owned != required {
                    return Err(format!(
                        "{} result ownership differs from its expected manifest selection",
                        step.tag()
                    ));
                }
            }
        }
        Ok(selected.into_iter().collect())
    }
}

/// The retired per-cell `backend_parity` key.
///
/// It held the ptrace reference run's cross-backend evidence, which
/// <https://github.com/rrnewton/hermit/issues/3301> removed: parity is now
/// measured only by the [`crate::parity`] post-pass, from the logs the
/// determinism cells retained. Every published schema-10 row carries the key
/// as `null`, and readers built before its removal still require it, so a
/// writer keeps emitting `null`. A reader accepts `null` or absence and
/// refuses any value, which excludes the pre-3301 rows that carried one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetiredBackendParity;

impl Serialize for RetiredBackendParity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_none()
    }
}

impl<'de> Deserialize<'de> for RetiredBackendParity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Null => Ok(Self),
            _ => Err(serde::de::Error::custom(
                "schema 10 cell carries retired backend parity evidence; rows written before https://github.com/rrnewton/hermit/issues/3301 removed it are excluded",
            )),
        }
    }
}

/// The retired `selected_backend_parity` population: always `[]`, for the
/// same reason and with the same reader rule as [`RetiredBackendParity`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetiredParityPopulation;

impl Serialize for RetiredParityPopulation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(std::iter::empty::<Value>())
    }
}

impl<'de> Deserialize<'de> for RetiredParityPopulation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Array(relations) if relations.is_empty() => Ok(Self),
            _ => Err(serde::de::Error::custom(
                "schema 10 evidence selects retired backend parity relations; rows written before https://github.com/rrnewton/hermit/issues/3301 removed them are excluded",
            )),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CellArtifactResultV10 {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
    pub cell_verdict: CellVerdict,
    pub backend_parity: RetiredBackendParity,
    /// The attempt ordinal this row's verdict was read from.
    ///
    /// Recorded so the artifact verifier can RE-DERIVE the ledger row's
    /// evidence binding independently instead of reading it back out of the
    /// row it is supposed to be checking. Without it the digest-bound check
    /// would compare the binding against itself and pass for any value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_attempt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_observation_history: Option<crate::cpu_evidence::CellCpuHistoryV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellArtifactResultV10Wire {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
    cell_verdict: CellVerdictV8,
    #[serde(default)]
    backend_parity: RetiredBackendParity,
    selected_attempt: u64,
    #[serde(default, deserialize_with = "crate::cpu_evidence::deserialize_present")]
    cpu_observation_history: Option<crate::cpu_evidence::CellCpuHistoryV1>,
}

impl<'de> Deserialize<'de> for CellArtifactResultV10 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = CellArtifactResultV10Wire::deserialize(deserializer)?;
        Ok(Self {
            lane: value.lane,
            category: value.category,
            test: value.test,
            mode: value.mode,
            backend: value.backend,
            cell_verdict: value.cell_verdict.into(),
            backend_parity: value.backend_parity,
            selected_attempt: Some(value.selected_attempt),
            cpu_observation_history: value.cpu_observation_history,
        })
    }
}

/// Exact pre-binding artifact shape. In particular a null/new ordinal is not
/// legacy absence. The full verdict implementation is shared below.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCellArtifactResultV10Wire {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
    cell_verdict: CellVerdictV8,
    #[serde(default)]
    backend_parity: RetiredBackendParity,
}

impl From<LegacyCellArtifactResultV10Wire> for CellArtifactResultV10 {
    fn from(value: LegacyCellArtifactResultV10Wire) -> Self {
        Self {
            lane: value.lane,
            category: value.category,
            test: value.test,
            mode: value.mode,
            backend: value.backend,
            cell_verdict: value.cell_verdict.into(),
            backend_parity: value.backend_parity,
            selected_attempt: None,
            cpu_observation_history: None,
        }
    }
}

impl CellArtifactResultV10 {
    pub fn identity(&self) -> CellIdentity {
        CellIdentity {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
        }
    }

    pub fn ordinary(&self) -> CellResult {
        CellResult {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
            cell_verdict: self.cell_verdict.clone(),
            // The per-cell ARTIFACT row carries no binding of its own; the
            // ledger row does. Left explicitly unbound rather than
            // reconstructed here, because a binding invented at a conversion
            // boundary is exactly the inferred foreign key this field exists
            // to replace.
            evidence_binding: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CellResultsEvidenceV10 {
    #[serde(skip_serializing_if = "CellBindingContract::is_legacy_unbound")]
    pub binding_contract: CellBindingContract,
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub selected: Vec<CellIdentity>,
    pub selected_backend_parity: RetiredParityPopulation,
    pub cells: Vec<CellResultV10>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResultsEvidenceV10Wire {
    // Only absence selects legacy. Present null is rejected by the typed
    // deserializer, rather than being collapsed into the default.
    #[serde(default)]
    binding_contract: CellBindingContract,
    path: ValidatePath,
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    selected_count: u64,
    recorded_count: u64,
    population_sha256: String,
    artifact: CellResultsArtifact,
    selected: Vec<CellIdentity>,
    #[serde(default)]
    selected_backend_parity: RetiredParityPopulation,
    cells: Vec<Value>,
}

impl<'de> Deserialize<'de> for CellResultsEvidenceV10 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = CellResultsEvidenceV10Wire::deserialize(deserializer)?;
        let cells = value
            .cells
            .into_iter()
            .map(|cell| match value.binding_contract {
                CellBindingContract::LegacyUnbound => {
                    serde_json::from_value::<LegacyCellResultV10Wire>(cell).map(Into::into)
                }
                CellBindingContract::SelectedAttemptV1 => serde_json::from_value(cell),
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            binding_contract: value.binding_contract,
            path: value.path,
            run_id: value.run_id,
            hermit_sha: value.hermit_sha,
            source_tree_dirty: value.source_tree_dirty,
            selected_count: value.selected_count,
            recorded_count: value.recorded_count,
            population_sha256: value.population_sha256,
            artifact: value.artifact,
            selected: value.selected,
            selected_backend_parity: value.selected_backend_parity,
            cells,
        })
    }
}

fn deserialize_verdict<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<CellVerdict, D::Error> {
    CellVerdictV8::deserialize(deserializer).map(Into::into)
}

/// The ledger contains summaries only. Raw invocation/report bytes live in the
/// bound artifact, so the parent ledger's path transport cannot rewrite them.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultV10 {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
    #[serde(deserialize_with = "deserialize_verdict")]
    pub cell_verdict: CellVerdict,
    #[serde(default)]
    pub backend_parity: RetiredBackendParity,
    /// The attempt ordinal this verdict was computed from, recorded on the
    /// LEDGER row as well as inside the binding.
    ///
    /// It is duplicated on purpose, and the purpose is narrow enough to state:
    /// it gives the decode-time guard a second operand. Without it the guard
    /// had nothing to compare the binding's attempt against, so it compared the
    /// binding against itself and accepted every ordinal. This does NOT
    /// establish that the recorded ordinal is the one the verdict came from --
    /// see `verify_cell_artifact_bytes` for what does, and does not, close
    /// that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_attempt: Option<u64>,
    /// The exact source attempt this verdict's evidence was read from.
    /// A compared legacy row is explicitly UNBOUND; a compared v1 row with
    /// this field absent is malformed, not a legacy fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_binding: Option<CellEvidenceBinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCellResultV10Wire {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
    #[serde(deserialize_with = "deserialize_verdict")]
    cell_verdict: CellVerdict,
    #[serde(default)]
    backend_parity: RetiredBackendParity,
}

impl From<LegacyCellResultV10Wire> for CellResultV10 {
    fn from(value: LegacyCellResultV10Wire) -> Self {
        Self {
            lane: value.lane,
            category: value.category,
            test: value.test,
            mode: value.mode,
            backend: value.backend,
            cell_verdict: value.cell_verdict,
            backend_parity: value.backend_parity,
            selected_attempt: None,
            evidence_binding: None,
        }
    }
}

impl CellResultV10 {
    pub fn identity(&self) -> CellIdentity {
        CellIdentity {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
        }
    }

    pub fn ordinary(&self) -> CellResult {
        CellResult {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
            cell_verdict: self.cell_verdict.clone(),
            evidence_binding: self.evidence_binding.clone(),
        }
    }
}

/// Keep exact reasons in the artifact and stable, path-free classifications in
/// the new ledger summary. No verdict or comparison field is changed.
pub fn compact_cell_verdict(verdict: &CellVerdict) -> CellVerdict {
    match verdict {
        CellVerdict::UnavailableWithReason {
            comparison_tier, ..
        } => CellVerdict::UnavailableWithReason {
            comparison_tier: comparison_tier.clone(),
            reason:
                "Comparison evidence unavailable; exact detail is retained in the cell artifact"
                    .into(),
        },
        CellVerdict::PerformsNoComparisonByDesign {
            comparison_tier, ..
        } => CellVerdict::PerformsNoComparisonByDesign {
            comparison_tier: comparison_tier.clone(),
            reason: "Mode performs no comparison by design".into(),
        },
        _ => verdict.clone(),
    }
}

impl CellArtifactResultV10 {
    /// Compact this artifact row into the terminal ledger record, binding a
    /// COMPARED verdict to the exact attempt it was computed from.
    ///
    /// The coordinates are parameters rather than fields so that no caller can
    /// produce a terminal record without stating which attempt it read. That
    /// is the whole point: an unbound compared verdict is exactly the row this
    /// work exists to stop being written.
    pub fn summary(&self, run_id: &str, hermit_sha: &str) -> Result<CellResultV10, String> {
        self.summary_for_contract(CellBindingContract::SelectedAttemptV1, run_id, hermit_sha)
    }

    /// A legacy summary retains the original observations, without inventing
    /// the attempt metadata that its producer did not record.
    pub fn summary_for_contract(
        &self,
        contract: CellBindingContract,
        run_id: &str,
        hermit_sha: &str,
    ) -> Result<CellResultV10, String> {
        match (contract, self.selected_attempt) {
            (CellBindingContract::LegacyUnbound, None)
            | (CellBindingContract::SelectedAttemptV1, Some(1..)) => {}
            _ => {
                return Err("schema 10 artifact attempt differs from its binding contract".into());
            }
        }
        let cell_verdict = compact_cell_verdict(&self.cell_verdict);
        // Only a compared verdict read an attempt. Binding a by-design or
        // unavailable verdict would name an event that was never published.
        let evidence_binding = self.selected_attempt.and_then(|attempt| {
            matches!(
                cell_verdict,
                CellVerdict::ComparedAndMatched { .. } | CellVerdict::ComparedAndDiverged { .. }
            )
            .then(|| {
                CellEvidenceBinding::for_validate_compared(
                    run_id,
                    &self.identity(),
                    hermit_sha,
                    attempt,
                )
            })
        });
        let recorded_attempt = evidence_binding
            .as_ref()
            .map(|binding| binding.selected_attempt);
        Ok(CellResultV10 {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
            cell_verdict,
            backend_parity: RetiredBackendParity,
            selected_attempt: recorded_attempt,
            evidence_binding,
        })
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ComparisonObservationVerdictV10 {
    Matched,
    Diverged,
    UnavailableWithReason { reason: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComparisonRelationV10 {
    /// A cell's own same-backend determinism comparison. The ptrace reference
    /// and cross-backend relations left with the reference run
    /// (<https://github.com/rrnewton/hermit/issues/3301>).
    Ordinary,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparisonObservationV10 {
    pub identity: CellIdentity,
    pub relation: ComparisonRelationV10,
    pub outer_attempt: Option<u64>,
    pub verdict: ComparisonObservationVerdictV10,
}

fn observation_verdict(verdict: &CellVerdict) -> ComparisonObservationVerdictV10 {
    match verdict {
        CellVerdict::ComparedAndMatched { .. } => ComparisonObservationVerdictV10::Matched,
        CellVerdict::ComparedAndDiverged { .. } => ComparisonObservationVerdictV10::Diverged,
        CellVerdict::PerformsNoComparisonByDesign { .. } => {
            ComparisonObservationVerdictV10::UnavailableWithReason {
                reason: "Mode performs no comparison by design".into(),
            }
        }
        CellVerdict::UnavailableWithReason { .. } => {
            ComparisonObservationVerdictV10::UnavailableWithReason {
                reason:
                    "Comparison evidence unavailable; exact detail is retained in the cell artifact"
                        .into(),
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum TestResultProducerSelectionV10 {
    Node { node: String },
    Compatibility,
}

#[derive(Clone, Debug, Serialize)]
pub struct VerifiedValidationEvidenceV10 {
    pub cell_results: CellResultsEvidenceV10,
    pub test_results: VerifiedTestResultsArtifactV9,
    pub observations: Vec<ComparisonObservationV10>,
    pub missing_cells: Vec<CellIdentity>,
    pub missing_test_producers: Vec<TestResultProducerSelectionV10>,
    pub full_test_results: bool,
}

/// Validate the existing canonical ordinary-comparison contract independently
/// of its carrier. Public derived operands use the same mode and INFO policy.
pub fn validate_ordinary_verdict(
    identity: &CellIdentity,
    verdict: &CellVerdict,
) -> Result<(), String> {
    match verdict {
        CellVerdict::ComparedAndMatched {
            comparison_tier,
            comparison,
            bitwise_parity,
            compared_log_messages,
        }
        | CellVerdict::ComparedAndDiverged {
            comparison_tier,
            comparison,
            bitwise_parity,
            compared_log_messages,
        } => {
            let expected_time = match identity.mode.as_str() {
                "verify" | "chaos" => true,
                "replay" => false,
                _ => return Err("schema 10 non-comparison mode carries a compared verdict".into()),
            };
            let canonical = *comparison_tier == ComparisonTier::CanonicalBitwise
                && comparison.is_canonical_bitwise_info_v1_for_time_policy(
                    expected_time,
                    compared_log_messages,
                )
                && *bitwise_parity == matches!(verdict, CellVerdict::ComparedAndMatched { .. });
            // A verify cell that declared the stripped comparator: a real
            // two-run comparison below L2, never bitwise parity, never the
            // canonical shape.
            let stripped = *comparison_tier == ComparisonTier::ExitAndStreamEquality
                && identity.mode == "verify"
                && comparison.is_stripped_verify_comparison(compared_log_messages)
                && !comparison.is_canonical_bitwise_info_v1_for_time_policy(
                    expected_time,
                    compared_log_messages,
                )
                && !*bitwise_parity;
            if !canonical && !stripped {
                return Err(
                    "schema 10 ordinary verdict contradicts its canonical comparison".into(),
                );
            }
        }
        CellVerdict::PerformsNoComparisonByDesign {
            comparison_tier,
            reason,
        } => {
            if !matches!(identity.mode.as_str(), "naked" | "custom")
                || *comparison_tier != ComparisonTier::DeclaredButUnverifiable
                || reason.trim().is_empty()
            {
                return Err("schema 10 no-comparison verdict contradicts its mode".into());
            }
        }
        CellVerdict::UnavailableWithReason {
            comparison_tier,
            reason,
        } => {
            if *comparison_tier != ComparisonTier::DeclaredButUnverifiable
                || reason.trim().is_empty()
            {
                return Err(
                    "schema 10 unavailable ordinary verdict has no reason or wrong tier".into(),
                );
            }
        }
    }
    Ok(())
}

impl CellResultsEvidenceV10 {
    /// Refuse cell evidence whose COMPARED verdicts are not each bound to a
    /// source attempt consistent with the row that carries them.
    ///
    /// WHAT THIS ESTABLISHES, stated exactly, because the previous version
    /// claimed an axis it did not check:
    ///
    /// * presence -- a compared verdict carries a binding, and a non-comparing
    ///   verdict does not;
    /// * run, tree and cell -- the binding belongs to this evidence;
    /// * attempt CONSISTENCY -- the binding's ordinal equals the ledger row's
    ///   own `selected_attempt`, which is a real second operand rather than
    ///   the same expression twice;
    /// * uniqueness -- two compared cells cannot bind the same attempt of the
    ///   same cell.
    ///
    /// WHAT IT DOES NOT ESTABLISH, and nothing in the ledger can: that the
    /// recorded ordinal is the attempt the verdict was actually computed from.
    /// `verify_cell_artifact_bytes` re-derives it from the digest-bound
    /// artifact, which protects it from later tampering but not from being
    /// wrong when written. It remains a producer assertion, and the end-to-end
    /// resolution against published rows is what would refute it.
    pub fn require_bound_compared_cells(&self) -> Result<(), String> {
        if self.binding_contract != CellBindingContract::SelectedAttemptV1 {
            return Err("schema 10 comparison evidence is legacy-unbound".into());
        }
        let mut seen = BTreeSet::new();
        for cell in &self.cells {
            let compared = matches!(
                cell.cell_verdict,
                CellVerdict::ComparedAndMatched { .. } | CellVerdict::ComparedAndDiverged { .. }
            );
            let identity = cell.identity();
            let named = CellEvidenceBinding::series_cell_key(&identity);
            if !compared {
                if cell.evidence_binding.is_some() || cell.selected_attempt.is_some() {
                    return Err(format!(
                        "cell {named} states no comparison yet carries an evidence binding"
                    ));
                }
                continue;
            }
            let binding = cell
                .evidence_binding
                .as_ref()
                .ok_or_else(|| format!("compared cell {named} carries no evidence binding"))?;
            binding.verify_against(&self.run_id, &self.hermit_sha, &identity)?;
            let recorded = cell.selected_attempt.ok_or_else(|| {
                format!("compared cell {named} carries a binding but no selected_attempt")
            })?;
            if binding.selected_attempt != recorded {
                return Err(format!(
                    "compared cell {named} binds attempt {} while the row records {recorded}",
                    binding.selected_attempt
                ));
            }
            if !seen.insert((named.clone(), binding.selected_attempt)) {
                return Err(format!(
                    "attempt {} of cell {named} is bound by more than one compared cell",
                    binding.selected_attempt
                ));
            }
        }
        Ok(())
    }

    /// The bound source attempts, for a reader that must count comparisons
    /// only from the binding.
    ///
    /// Deliberately NOT a list of published `event_id`s. Predicting one is
    /// unsound for a collapsible row, which every compared verdict is; a
    /// reader holding the published rows resolves each attempt by interval.
    pub fn bound_attempts(&self) -> Result<BTreeSet<(String, u64)>, String> {
        self.require_bound_compared_cells()?;
        Ok(self
            .cells
            .iter()
            .filter_map(|cell| cell.evidence_binding.as_ref())
            .map(|binding| (binding.series_cell.clone(), binding.selected_attempt))
            .collect())
    }

    pub fn ordinary_evidence(&self) -> CellResultsEvidence {
        CellResultsEvidence {
            run_id: self.run_id.clone(),
            hermit_sha: self.hermit_sha.clone(),
            source_tree_dirty: self.source_tree_dirty,
            selected_count: self.selected_count,
            recorded_count: self.recorded_count,
            population_sha256: self.population_sha256.clone(),
            artifact: self.artifact.clone(),
            selected: self.selected.clone(),
            cells: self.cells.iter().map(CellResultV10::ordinary).collect(),
        }
    }

    fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        if row.run_id.as_deref() != Some(&self.run_id)
            || !nonblank_component(&self.run_id)
            || row.commit.as_deref() != Some(&self.hermit_sha)
            || !is_lower_hex(&self.hermit_sha, 40)
            || row.profile.as_deref() != Some(self.path.as_str())
            || row.tree_dirty != Some(false)
            || self.source_tree_dirty
        {
            return Err("schema 10 cell evidence differs from the exact clean row identity".into());
        }
        if self.selected_count != self.selected.len() as u64
            || self.recorded_count != self.cells.len() as u64
            || self.artifact.row_count != self.recorded_count
            || self.recorded_count > self.selected_count
            || !self.selected.windows(2).all(|pair| pair[0] < pair[1])
            || !self
                .cells
                .windows(2)
                .all(|pair| pair[0].identity() < pair[1].identity())
        {
            return Err(
                "schema 10 cell populations are inconsistent, duplicate, or unsorted".into(),
            );
        }
        for identity in &self.selected {
            validate_identity(identity)?;
        }
        // Old observations remain readable, but cannot satisfy bound evidence.
        // The new contract never permits a missing comparison binding.
        match self.binding_contract {
            CellBindingContract::SelectedAttemptV1 => self.require_bound_compared_cells()?,
            CellBindingContract::LegacyUnbound => {
                if self
                    .cells
                    .iter()
                    .any(|cell| cell.selected_attempt.is_some() || cell.evidence_binding.is_some())
                {
                    return Err("legacy schema 10 cell carries binding fields".into());
                }
            }
        }
        let population = serde_json::to_vec(
            &serde_json::to_value(&self.selected).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if self.population_sha256 != hex_digest(&population)
            || self.artifact.path
                != format!(
                    "ignored/validate/artifacts/{}/cell-results.jsonl",
                    self.run_id
                )
            || !is_lower_hex(&self.artifact.sha256, 64)
        {
            return Err("schema 10 cell population or artifact identity is malformed".into());
        }
        let selected = self.selected.iter().collect::<BTreeSet<_>>();
        for cell in &self.cells {
            let identity = cell.identity();
            if !selected.contains(&identity) {
                return Err("schema 10 recorded an unselected cell".into());
            }
            validate_ordinary_verdict(&identity, &cell.cell_verdict)?;
            if cell.cell_verdict != compact_cell_verdict(&cell.cell_verdict) {
                return Err(
                    "schema 10 ledger ordinary reason is not the stable artifact summary".into(),
                );
            }
        }
        Ok(())
    }
}

impl HistoryRow {
    pub fn schema10_cell_results(&self) -> Result<Option<CellResultsEvidenceV10>, String> {
        if self.schema_version != Some(VALIDATION_EVIDENCE_SCHEMA_VERSION) {
            return Ok(None);
        }
        let value = self
            .cell_results
            .as_ref()
            .ok_or("schema 10 row omitted cell_results")?;
        let raw = serde_json::to_value(value).map_err(|error| error.to_string())?;
        let evidence: CellResultsEvidenceV10 = serde_json::from_value(raw)
            .map_err(|error| format!("invalid schema 10 cell_results: {error}"))?;
        evidence.validate_for_row(self)?;
        Ok(Some(evidence))
    }

    pub fn constructed_plan_artifact(&self) -> Result<Option<ConstructedPlanArtifact>, String> {
        if self.schema_version != Some(VALIDATION_EVIDENCE_SCHEMA_VERSION) {
            return Ok(None);
        }
        let value = self
            .extra
            .get("constructed_plan")
            .ok_or("schema 10 row omitted constructed_plan")?;
        let reference: ConstructedPlanArtifact = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid schema 10 constructed_plan: {error}"))?;
        let run_id = self
            .run_id
            .as_deref()
            .ok_or("schema 10 row omitted run_id")?;
        if !nonblank_component(run_id)
            || reference.path
                != format!("ignored/validate/artifacts/{run_id}/constructed-plan.json")
            || reference.bytes == 0
            || !is_lower_hex(&reference.sha256, 64)
        {
            return Err("schema 10 constructed plan reference is malformed".into());
        }
        Ok(Some(reference))
    }

    pub fn verify_schema10_artifact_bytes(
        &self,
        plan_bytes: &[u8],
        cell_bytes: &[u8],
        test_bytes: &[u8],
    ) -> Result<Option<VerifiedValidationEvidenceV10>, String> {
        let Some(reference) = self.constructed_plan_artifact()? else {
            return Ok(None);
        };
        if reference.bytes != plan_bytes.len() as u64 || reference.sha256 != hex_digest(plan_bytes)
        {
            return Err(
                "schema 10 constructed plan bytes differ from their recorded identity".into(),
            );
        }
        let plan: ConstructedValidationPlanV10 = serde_json::from_slice(plan_bytes)
            .map_err(|error| format!("invalid schema 10 constructed plan artifact: {error}"))?;
        plan.path.validate_selected_scope(self)?;
        if self.run_id.as_deref() != Some(&plan.run_id)
            || self.commit.as_deref() != Some(&plan.hermit_sha)
            || self.profile.as_deref() != Some(plan.path.as_str())
        {
            return Err("schema 10 constructed plan differs from its row identity".into());
        }
        let cfg = plan.constructed_dag()?;
        let selected_tests = TestResultsSelectedPopulation::from_constructed_plan_steps(
            &cfg.steps,
            plan.compatibility_selected,
        )?;
        let test_evidence = self
            .test_results
            .as_ref()
            .ok_or("schema 10 row omitted test_results")?
            .schema9()?;
        test_evidence.validate_for_row(self)?;
        let test_results = test_evidence.verify_artifact_bytes(&selected_tests, test_bytes)?;
        let evidence = self
            .schema10_cell_results()?
            .expect("schema 10 dispatch established");
        let planned_cells = plan.planned_cells()?;
        if evidence.selected != planned_cells {
            return Err(
                "schema 10 cell population differs from the independently retained plan".into(),
            );
        }
        let artifact_cells = evidence.verify_cell_artifact_bytes(cell_bytes)?;
        let recorded = artifact_cells
            .iter()
            .map(CellArtifactResultV10::identity)
            .collect::<BTreeSet<_>>();
        let missing_cells = planned_cells
            .into_iter()
            .filter(|cell| !recorded.contains(cell))
            .collect();
        let observations = artifact_cells
            .iter()
            .filter(|cell| {
                !matches!(
                    cell.cell_verdict,
                    CellVerdict::PerformsNoComparisonByDesign { .. }
                )
            })
            .map(|cell| ComparisonObservationV10 {
                identity: cell.identity(),
                relation: ComparisonRelationV10::Ordinary,
                outer_attempt: None,
                verdict: observation_verdict(&cell.cell_verdict),
            })
            .collect();
        Ok(Some(VerifiedValidationEvidenceV10 {
            cell_results: evidence,
            test_results,
            observations,
            missing_cells,
            // The independent V9 artifact verifier above requires every
            // selected producer. Success therefore implies full coverage.
            missing_test_producers: Vec::new(),
            full_test_results: true,
        }))
    }
}

impl CellResultsEvidenceV10 {
    /// Verify canonical artifact bytes, then derive every compact ledger value
    /// from the full retained attempts. A caller cannot qualify a summary alone.
    pub fn verify_cell_artifact_bytes(
        &self,
        bytes: &[u8],
    ) -> Result<Vec<CellArtifactResultV10>, String> {
        if self.artifact.sha256 != hex_digest(bytes)
            || (!bytes.is_empty() && !bytes.ends_with(b"\n"))
        {
            return Err("schema 10 cell artifact hash or final newline is invalid".into());
        }
        let mut cells = Vec::new();
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            let line = &line[..line.len() - 1];
            let mut value: Value = serde_json::from_slice(line)
                .map_err(|error| format!("invalid schema 10 cell artifact row: {error}"))?;
            if serde_json::to_vec(&value).map_err(|error| error.to_string())? != line {
                return Err("schema 10 cell artifact row is not canonical JSON".into());
            }
            let object = value
                .as_object_mut()
                .ok_or("schema 10 cell artifact row is not an object")?;
            if object.remove("run_id") != Some(Value::String(self.run_id.clone()))
                || object.remove("hermit_sha") != Some(Value::String(self.hermit_sha.clone()))
                || object.remove("source_tree_dirty") != Some(Value::Bool(false))
            {
                return Err(
                    "schema 10 cell artifact row has a different run or source identity".into(),
                );
            }
            let cell: CellArtifactResultV10 = match self.binding_contract {
                CellBindingContract::LegacyUnbound => {
                    serde_json::from_value::<LegacyCellArtifactResultV10Wire>(value).map(Into::into)
                }
                CellBindingContract::SelectedAttemptV1 => serde_json::from_value(value),
            }
            .map_err(|error| format!("invalid schema 10 full cell evidence: {error}"))?;
            validate_ordinary_verdict(&cell.identity(), &cell.cell_verdict)?;
            if let Some(history) = &cell.cpu_observation_history {
                history.validate_for_artifact(
                    &self.run_id,
                    &self.hermit_sha,
                    &cell.identity(),
                    cell.selected_attempt
                        .ok_or("CPU history lacks selected-attempt binding")?,
                )?;
            }
            cells.push(cell);
        }
        let summaries = cells
            .iter()
            .map(|cell| {
                cell.summary_for_contract(self.binding_contract, &self.run_id, &self.hermit_sha)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if summaries != self.cells
            || cells.len() as u64 != self.recorded_count
            || cells.len() as u64 != self.artifact.row_count
        {
            return Err("schema 10 cell artifact differs from its compact ledger summary".into());
        }
        Ok(cells)
    }
}

#[cfg(test)]
mod raw_input_census_tests {
    use super::*;

    fn input(attempt: u64, payload: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema":4,"run_id":"partial-run","hermit_sha":"a".repeat(40),
            "source_tree_dirty":false,"lane":"portable","category":"fixture",
            "test":"fixture/failed","mode":"verify","backend":"ptrace",
            "attempt":attempt,"outcome":"FAIL","payload":payload,
        }))
        .unwrap()
    }

    fn history(schema: u32) -> HistoryRow {
        serde_json::from_value(serde_json::json!({
            "schema_version":schema,"run_id":"partial-run","commit":"a".repeat(40),
            "tree_dirty":false,"result":"fail","executed_tests":null,
            "gates_expected":3,"gates_run":1,"unaccounted_nodes":["test.not-run"],
        }))
        .unwrap()
    }

    #[test]
    fn producer_census_retains_partial_failure_and_refuses_missing_attempts_or_changed_bytes() {
        let mut attempts = input(1, "first failure");
        attempts.push(b'\n');
        attempts.extend(input(2, "second failure"));
        attempts.push(b'\n');
        let inputs = BTreeMap::from([
            ("one/results.jsonl".into(), attempts),
            ("empty/results.jsonl".into(), Vec::new()),
        ]);
        let row = history(5);
        let census =
            RawResultInputCensusV1::from_inputs("partial-run", &"a".repeat(40), &inputs).unwrap();
        census.verify_inputs(&row, &inputs).unwrap();
        assert_eq!(row.result.as_deref(), Some("fail"));
        assert_eq!(row.executed_tests, None);
        assert_eq!(row.gates_expected, Some(3));
        assert_eq!(row.gates_run, Some(1));
        assert_eq!(
            census.files[1]
                .rows
                .iter()
                .map(|row| row.attempt)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        for change in [
            "missing-file",
            "missing-empty-file",
            "superseded-attempt",
            "truncated",
            "changed-payload",
            "extra-file",
            "renamed-file",
        ] {
            let mut changed = inputs.clone();
            match change {
                "missing-file" => {
                    changed.remove("one/results.jsonl");
                }
                "missing-empty-file" => {
                    changed.remove("empty/results.jsonl");
                }
                "superseded-attempt" => {
                    changed.insert("one/results.jsonl".into(), input(2, "second failure"));
                }
                "truncated" => {
                    changed.get_mut("one/results.jsonl").unwrap().pop();
                }
                "changed-payload" => {
                    let bytes = changed.get_mut("one/results.jsonl").unwrap();
                    *bytes = String::from_utf8(bytes.clone())
                        .unwrap()
                        .replace("first failure", "FIRST FAILURE")
                        .into_bytes();
                }
                "extra-file" => {
                    changed.insert("extra/results.jsonl".into(), Vec::new());
                }
                "renamed-file" => {
                    let bytes = changed.remove("one/results.jsonl").unwrap();
                    changed.insert("other/results.jsonl".into(), bytes);
                }
                _ => unreachable!(),
            }
            assert!(
                census.verify_inputs(&row, &changed).is_err(),
                "admitted {change}"
            );
        }
        let mut repeated = inputs.clone();
        repeated.insert("duplicate/results.jsonl".into(), input(1, "first failure"));
        assert!(
            RawResultInputCensusV1::from_inputs("partial-run", &"a".repeat(40), &repeated).is_err()
        );
        let mut duplicate_field = input(1, "first failure");
        duplicate_field.pop();
        duplicate_field.extend(b",\"attempt\":2}");
        assert!(
            RawResultInputCensusV1::from_inputs(
                "partial-run",
                &"a".repeat(40),
                &BTreeMap::from([("results.jsonl".into(), duplicate_field)])
            )
            .is_err()
        );
    }

    #[test]
    fn census_extension_preserves_historical_absence_and_refuses_unknown_present_shapes() {
        for schema in [5, 10] {
            let row = history(schema);
            let encoded = serde_json::to_vec(&row).unwrap();
            assert!(row.raw_result_input_census_v1().unwrap().is_none());
            assert_eq!(serde_json::to_vec(&row).unwrap(), encoded);
            let inputs = BTreeMap::from([("results.jsonl".into(), input(1, "failure"))]);
            let census =
                RawResultInputCensusV1::from_inputs("partial-run", &"a".repeat(40), &inputs)
                    .unwrap();
            let mut value = serde_json::to_value(&row).unwrap();
            value["raw_result_input_census_v1"] = serde_json::to_value(&census).unwrap();
            let current: HistoryRow = serde_json::from_value(value.clone()).unwrap();
            current
                .raw_result_input_census_v1()
                .unwrap()
                .unwrap()
                .verify_inputs(&current, &inputs)
                .unwrap();
            for mutation in [
                "null",
                "version",
                "unknown-field",
                "wrong-run",
                "wrong-source",
                "unsafe-path",
                "repeated-file",
                "zero-attempt",
                "error-and-proof",
            ] {
                let mut changed = value.clone();
                let proof = &mut changed["raw_result_input_census_v1"];
                match mutation {
                    "null" => *proof = Value::Null,
                    "version" => proof["schema"] = Value::from(2),
                    "unknown-field" => proof["accept_partial"] = Value::Bool(true),
                    "wrong-run" => proof["run_id"] = Value::String("another-run".into()),
                    "wrong-source" => proof["hermit_sha"] = Value::String("b".repeat(40)),
                    "unsafe-path" => {
                        proof["files"][0]["path"] = Value::String("../results.jsonl".into())
                    }
                    "repeated-file" => {
                        let file = proof["files"][0].clone();
                        proof["files"].as_array_mut().unwrap().push(file);
                    }
                    "zero-attempt" => proof["files"][0]["rows"][0]["attempt"] = Value::from(0),
                    "error-and-proof" => {
                        changed["raw_result_input_census_error"] =
                            Value::String("unreadable".into())
                    }
                    _ => unreachable!(),
                }
                let changed: HistoryRow = serde_json::from_value(changed).unwrap();
                assert!(
                    changed.raw_result_input_census_v1().is_err(),
                    "admitted {schema}/{mutation}"
                );
            }
        }
        let empty =
            RawResultInputCensusV1::from_inputs("partial-run", &"a".repeat(40), &BTreeMap::new())
                .unwrap();
        empty.verify_inputs(&history(5), &BTreeMap::new()).unwrap();
        // This authenticates an empty raw census only. Schema 5 supplies no
        // selected-zero evidence and cannot authorize zero-current completion.
        assert!(history(5).constructed_plan_artifact().unwrap().is_none());
    }
}
