//! Cumulative validation evidence with an independently retained plan and
//! separate ordinary, reference, and cross-backend comparisons.

use dagrun::io::dag_from_json;
use dagrun::io::dag_to_json;
use dagrun::model::DagConfig;
use dagrun::model::DagManifest;

use super::*;
use crate::backend_parity::BackendParityReport;
use crate::backend_parity::BackendParityVerdict;
use crate::canonical_verdict::Verdict;
use crate::canonical_verdict::VerificationReport;
use crate::canonical_verdict::VerificationRuntime;
use crate::logdiff_report::RecordEnvelopePolicy;
use crate::runner::AttemptResult;

pub const VALIDATION_EVIDENCE_SCHEMA_VERSION: u32 = 10;

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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct BackendParityRelation {
    pub candidate: CellIdentity,
    pub reference_backend: String,
    pub record_envelope: RecordEnvelopePolicy,
}

impl BackendParityRelation {
    pub fn ptrace(candidate: CellIdentity) -> Self {
        Self {
            candidate,
            reference_backend: "ptrace".into(),
            record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.candidate.mode != "verify"
            || self.candidate.backend == "ptrace"
            || self.reference_backend != "ptrace"
            || self.record_envelope != RecordEnvelopePolicy::CrossBackendDetcoreV1
        {
            return Err(
                "schema 10 backend parity relation has unsupported operands or policy".into(),
            );
        }
        validate_identity(&self.candidate)
    }
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

    fn populations(&self) -> Result<(Vec<CellIdentity>, Vec<BackendParityRelation>), String> {
        let cfg = self.constructed_dag()?;
        let expected =
            crate::validation_dag::expected_cells_from_json(&self.expected_e2e_plan_json)?
                .iter()
                .map(exact_identity)
                .collect::<Result<BTreeSet<_>, _>>()?;
        let mut selected = BTreeSet::new();
        let mut relations = BTreeSet::new();
        let mut tags = BTreeSet::new();
        for step in &cfg.steps {
            if !tags.insert(step.tag()) {
                return Err("constructed plan repeats a step identity".into());
            }
            let parity = crate::backend_parity_policy::selects_ptrace_parity(step)?;
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
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if owned != required {
                    return Err(format!(
                        "{} result ownership differs from its expected manifest selection",
                        step.tag()
                    ));
                }
            }
            if parity {
                for cell in owned
                    .into_iter()
                    .filter(|cell| cell.mode == "verify" && cell.backend != "ptrace")
                {
                    if !relations.insert(BackendParityRelation::ptrace(cell)) {
                        return Err(
                            "constructed plan selects one backend parity relation more than once"
                                .into(),
                        );
                    }
                }
            }
        }
        Ok((
            selected.into_iter().collect(),
            relations.into_iter().collect(),
        ))
    }

    pub fn planned_cells(&self) -> Result<Vec<CellIdentity>, String> {
        self.populations().map(|(cells, _)| cells)
    }

    pub fn planned_backend_parity_relations(&self) -> Result<Vec<BackendParityRelation>, String> {
        self.populations().map(|(_, relations)| relations)
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
    pub backend_parity: RequiredNullable<CellBackendParity>,
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
    backend_parity: RequiredNullable<CellBackendParity>,
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
        })
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
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellResultsEvidenceV10 {
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub selected: Vec<CellIdentity>,
    pub selected_backend_parity: Vec<BackendParityRelation>,
    pub cells: Vec<CellResultV10>,
}

fn deserialize_verdict<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<CellVerdict, D::Error> {
    CellVerdictV8::deserialize(deserializer).map(Into::into)
}

fn deserialize_nullable_verdict<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<RequiredNullable<CellVerdict>, D::Error> {
    Ok(
        match RequiredNullable::<CellVerdictV8>::deserialize(deserializer)? {
            RequiredNullable::Null => RequiredNullable::Null,
            RequiredNullable::Value(verdict) => RequiredNullable::Value(verdict.into()),
        },
    )
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
    pub backend_parity: RequiredNullable<CellBackendParitySummary>,
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
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellBackendParitySummary {
    pub reference_backend: String,
    pub record_envelope: RecordEnvelopePolicy,
    pub attempts: Vec<BackendParityAttemptSummary>,
}

impl CellBackendParitySummary {
    /// Apply only after the full artifact verifier has authenticated this
    /// summary. A matching retry is measured evidence, not a clean first pass.
    pub fn is_clean_match(&self) -> bool {
        self.reference_backend == "ptrace"
            && self.record_envelope == RecordEnvelopePolicy::CrossBackendDetcoreV1
            && matches!(self.attempts.as_slice(), [attempt] if attempt.attempt == 1
                && matches!(attempt.candidate, CellVerdict::ComparedAndMatched { .. })
                && matches!(attempt.reference, RequiredNullable::Value(CellVerdict::ComparedAndMatched { .. }))
                && attempt.cross == ComparisonObservationVerdictV10::Matched
                && matches!(&attempt.candidate_verification_report_sha256, RequiredNullable::Value(value) if is_lower_hex(value, 64))
                && matches!(&attempt.reference_verification_report_sha256, RequiredNullable::Value(value) if is_lower_hex(value, 64)))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackendParityAttemptSummary {
    pub attempt: u64,
    #[serde(deserialize_with = "deserialize_verdict")]
    pub candidate: CellVerdict,
    #[serde(deserialize_with = "deserialize_nullable_verdict")]
    pub reference: RequiredNullable<CellVerdict>,
    pub cross: ComparisonObservationVerdictV10,
    pub candidate_verification_report_sha256: RequiredNullable<String>,
    pub reference_verification_report_sha256: RequiredNullable<String>,
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
    pub fn summary(&self) -> Result<CellResultV10, String> {
        let backend_parity = match &self.backend_parity {
            RequiredNullable::Null => RequiredNullable::Null,
            RequiredNullable::Value(parity) => {
                if parity.candidate_verdict(&self.identity())? != self.cell_verdict {
                    return Err("schema 10 cell ordinary verdict differs from the selected candidate attempt".into());
                }
                RequiredNullable::Value(parity.summary(&self.identity())?)
            }
        };
        Ok(CellResultV10 {
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self.backend.clone(),
            cell_verdict: compact_cell_verdict(&self.cell_verdict),
            backend_parity,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellBackendParity {
    pub reference_backend: String,
    pub record_envelope: RecordEnvelopePolicy,
    pub attempts: Vec<BackendParityCellAttempt>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BackendParityCellAttempt {
    Completed {
        attempt: u64,
        candidate_attempt: ParityAttempt,
        reference_attempt: ParityAttempt,
        report: BackendParityReport,
    },
    UnavailableWithReason {
        attempt: u64,
        candidate_attempt: ParityAttempt,
        reference_attempt: RequiredNullable<ParityAttempt>,
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct ParityAttempt(pub AttemptResult);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParityAttemptWire {
    index: String,
    outcome: String,
    error_kind: RequiredNullable<String>,
    status: RequiredNullable<i32>,
    signal: RequiredNullable<i32>,
    timed_out: bool,
    duration_ms: u128,
    cpu_usage_usec: RequiredNullable<u64>,
    observation_sha256: RequiredNullable<String>,
    argv: Vec<String>,
    guest_argv: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: String,
    shell_command: String,
    stdout: String,
    stderr: String,
    verification_report: RequiredNullable<String>,
    verification_report_sha256: RequiredNullable<String>,
    runtime: RequiredNullable<VerificationRuntime>,
    first_divergent_scheduler_turn: RequiredNullable<u64>,
    first_divergent_virtual_nanoseconds: RequiredNullable<u64>,
    first_divergent_record: RequiredNullable<u64>,
    first_divergent_syscall: RequiredNullable<u64>,
    first_divergent_left_message: RequiredNullable<String>,
    first_divergent_right_message: RequiredNullable<String>,
    sabre_path_evidence: RequiredNullable<String>,
    sabre_path_evidence_sha256: RequiredNullable<String>,
    reason: RequiredNullable<String>,
}

impl<'de> Deserialize<'de> for ParityAttempt {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Serde's buffered tagged/untagged enum content does not implement
        // deserialize_u128. Keep duration_ms an integer and re-enter the JSON
        // value deserializer, while refusing duplicate keys before buffering.
        struct AttemptObject;
        impl<'de> serde::de::Visitor<'de> for AttemptObject {
            type Value = Value;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an exact parity attempt object")
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Value, A::Error> {
                let mut fields = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if fields.insert(key.clone(), value).is_some() {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate parity attempt field {key}"
                        )));
                    }
                }
                Ok(Value::Object(fields))
            }
        }
        let value = deserializer.deserialize_map(AttemptObject)?;
        // Value cannot represent every u128 without arbitrary_precision. Refuse
        // overflow and floating-point encodings explicitly; never round a wire
        // duration. Historical AttemptResult parsing remains unchanged.
        if value.get("duration_ms").and_then(Value::as_u64).is_none() {
            return Err(serde::de::Error::custom(
                "parity attempt duration_ms must be an exact unsigned 64-bit integer",
            ));
        }
        let value: ParityAttemptWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(Self(AttemptResult {
            index: value.index,
            outcome: value.outcome,
            error_kind: required_nullable_into_option(value.error_kind),
            status: required_nullable_into_option(value.status),
            signal: required_nullable_into_option(value.signal),
            timed_out: value.timed_out,
            duration_ms: value.duration_ms,
            cpu_usage_usec: required_nullable_into_option(value.cpu_usage_usec),
            observation_sha256: required_nullable_into_option(value.observation_sha256),
            argv: value.argv,
            guest_argv: value.guest_argv,
            env: value.env,
            cwd: value.cwd,
            shell_command: value.shell_command,
            stdout: value.stdout,
            stderr: value.stderr,
            verification_report: required_nullable_into_option(value.verification_report),
            verification_report_sha256: required_nullable_into_option(
                value.verification_report_sha256,
            ),
            runtime: required_nullable_into_option(value.runtime),
            first_divergent_scheduler_turn: required_nullable_into_option(
                value.first_divergent_scheduler_turn,
            ),
            first_divergent_virtual_nanoseconds: required_nullable_into_option(
                value.first_divergent_virtual_nanoseconds,
            ),
            first_divergent_record: required_nullable_into_option(value.first_divergent_record),
            first_divergent_syscall: required_nullable_into_option(value.first_divergent_syscall),
            first_divergent_left_message: required_nullable_into_option(
                value.first_divergent_left_message,
            ),
            first_divergent_right_message: required_nullable_into_option(
                value.first_divergent_right_message,
            ),
            sabre_path_evidence: required_nullable_into_option(value.sabre_path_evidence),
            sabre_path_evidence_sha256: required_nullable_into_option(
                value.sabre_path_evidence_sha256,
            ),
            reason: required_nullable_into_option(value.reason),
        }))
    }
}

fn unavailable(reason: impl Into<String>) -> CellVerdict {
    CellVerdict::UnavailableWithReason {
        comparison_tier: ComparisonTier::DeclaredButUnverifiable,
        reason: reason.into(),
    }
}

impl ParityAttempt {
    /// Decode the current attempt shape without changing historical runner
    /// deserialization. In particular, every nullable field must be present.
    pub fn from_value(value: Value) -> Result<Self, String> {
        serde_json::from_value(value)
            .map_err(|error| format!("invalid schema 10 parity attempt: {error}"))
    }

    fn bind_role(&self, backend: &str, index: &str) -> Result<(), String> {
        let attempt = &self.0;
        if attempt.index != index || attempt.cwd.is_empty() || attempt.shell_command.is_empty() {
            return Err("schema 10 parity attempt has incomplete invocation identity".into());
        }
        let separator = attempt
            .argv
            .iter()
            .position(|arg| arg == "--")
            .ok_or("schema 10 parity invocation omitted its guest separator")?;
        let prefix = &attempt.argv[..separator];
        let mut backends = Vec::new();
        for (position, argument) in prefix.iter().enumerate() {
            if argument == "--backend" {
                backends.push(
                    prefix
                        .get(position + 1)
                        .map(String::as_str)
                        .ok_or("schema 10 parity invocation has an incomplete backend option")?,
                );
            } else if let Some(value) = argument.strip_prefix("--backend=") {
                backends.push(value);
            }
        }
        if backends != [backend]
            || attempt.argv[separator + 1..] != attempt.guest_argv
            || attempt.guest_argv.is_empty()
            || !prefix.iter().any(|arg| arg == "--verify")
            || !prefix.iter().any(|arg| arg == "--verify-strict")
        {
            return Err(
                "schema 10 parity invocation contradicts its backend, guest, or strict verify role"
                    .into(),
            );
        }
        Ok(())
    }

    fn report(&self) -> Result<Option<VerificationReport>, String> {
        let attempt = &self.0;
        let Some(raw) = &attempt.verification_report else {
            if attempt.verification_report_sha256.is_some()
                || attempt.runtime.is_some()
                || attempt.first_divergent_record.is_some()
                || attempt.first_divergent_syscall.is_some()
                || attempt.first_divergent_scheduler_turn.is_some()
                || attempt.first_divergent_virtual_nanoseconds.is_some()
                || attempt.first_divergent_left_message.is_some()
                || attempt.first_divergent_right_message.is_some()
            {
                return Err("schema 10 missing verification report carries contradictory comparison evidence".into());
            }
            return Ok(None);
        };
        if attempt.verification_report_sha256.as_deref()
            != Some(hex_digest(raw.as_bytes()).as_str())
        {
            return Err(
                "schema 10 verification report SHA256 differs from its exact retained bytes".into(),
            );
        }
        let report = VerificationReport::from_current_json_slice(raw.as_bytes())?;
        if attempt.runtime != report.runtime
            || attempt.first_divergent_record != report.first_divergent_record
            || attempt.first_divergent_syscall != report.first_divergent_syscall
            || attempt.first_divergent_scheduler_turn != report.first_divergent_scheduler_turn
            || attempt.first_divergent_virtual_nanoseconds
                != report.first_divergent_virtual_nanoseconds
            || attempt.first_divergent_left_message != report.first_divergent_left_message
            || attempt.first_divergent_right_message != report.first_divergent_right_message
        {
            return Err(
                "schema 10 attempt runtime or divergence coordinates differ from its report".into(),
            );
        }
        Ok(Some(report))
    }

    fn path_eligible(&self, backend: &str) -> Result<bool, String> {
        let attempt = &self.0;
        match (
            &attempt.sabre_path_evidence,
            &attempt.sabre_path_evidence_sha256,
        ) {
            (Some(raw), Some(digest)) if hex_digest(raw.as_bytes()) == *digest => {}
            (None, None) => {}
            _ => {
                return Err(
                    "schema 10 backend path evidence identity differs from retained bytes".into(),
                );
            }
        }
        if backend != "sabre" {
            if attempt.sabre_path_evidence.is_some() {
                return Err("schema 10 non-SaBRe operand carries SaBRe path evidence".into());
            }
            return Ok(true);
        }
        // Normalize only the already checked option spelling for the existing
        // producer-owned path checker. No report or guest byte is changed.
        let mut normalized = attempt.clone();
        normalized.argv = normalized
            .argv
            .iter()
            .flat_map(|arg| {
                if arg == "--backend=sabre" {
                    vec!["--backend".into(), "sabre".into()]
                } else {
                    vec![arg.clone()]
                }
            })
            .collect();
        crate::runner::summarize_sabre_path_evidence(&[normalized])
            .map(|summary| summary.is_some_and(|value| value["eligible"] == true))
    }

    /// The candidate and reference keep their actual ordinary comparison
    /// verdicts. A path refusal is considered before interpreting a divergence.
    pub fn ordinary_verdict(&self, backend: &str, index: &str) -> Result<CellVerdict, String> {
        self.bind_role(backend, index)?;
        let attempt = &self.0;
        let report = self.report()?;
        if !self.path_eligible(backend)? {
            return Ok(unavailable(
                "Backend execution path is ineligible; exact evidence is retained",
            ));
        }
        if attempt.status.is_some() && attempt.signal.is_some() {
            return Err("schema 10 attempt records both exit status and signal".into());
        }
        let reason = || {
            attempt
                .reason
                .clone()
                .unwrap_or_else(|| "Attempt produced no completed canonical comparison".into())
        };
        let Some(report) = report else {
            let stopped = attempt.status.is_some_and(|status| status != 0)
                || attempt.signal.is_some_and(|signal| signal > 0)
                || (attempt.timed_out && attempt.status.is_none() && attempt.signal.is_none());
            if attempt.outcome != "ERROR"
                || !stopped
                || attempt.error_kind.as_deref().is_none_or(str::is_empty)
            {
                return Err("schema 10 missing report lacks an explicit failed or stopped process disposition".into());
            }
            return Ok(unavailable(reason()));
        };
        if !matches!(report.verdict, Verdict::Matched | Verdict::Diverged) {
            if attempt.outcome == "PASS" {
                return Err("schema 10 PASS has no completed ordinary comparison".into());
            }
            return Ok(unavailable(reason()));
        }
        if attempt.timed_out {
            if attempt.outcome == "PASS" {
                return Err("schema 10 timed-out attempt claims PASS".into());
            }
            return Ok(unavailable(reason()));
        }
        report.require_canonical_comparison()?;
        let raw: Value =
            serde_json::from_str(attempt.verification_report.as_ref().expect("report exists"))
                .map_err(|error| error.to_string())?;
        let comparison: ComparisonSpec = serde_json::from_value(raw["comparison"].clone())
            .map_err(|error| format!("schema 10 ordinary comparison is malformed: {error}"))?;
        let counts: RequiredNullable<ComparedLogCounts> =
            serde_json::from_value(raw["compared_log_messages"].clone()).map_err(|error| {
                format!("schema 10 ordinary comparison counts are malformed: {error}")
            })?;
        if !comparison.is_canonical_bitwise_info_v1(&counts) {
            return Ok(unavailable(
                "Ordinary comparison does not satisfy BitwiseInfoV1/all_records_v1",
            ));
        }
        if report.verdict == Verdict::Matched {
            report.require_canonical_match()?;
            report.require_exact_output_match()?;
            if attempt.outcome != "PASS"
                || attempt.status != Some(0)
                || attempt.signal.is_some()
                || attempt.error_kind.is_some()
            {
                return Err("schema 10 ordinary match contradicts its process result".into());
            }
            Ok(CellVerdict::ComparedAndMatched {
                comparison_tier: ComparisonTier::CanonicalBitwise,
                comparison,
                bitwise_parity: true,
                compared_log_messages: counts,
            })
        } else {
            if attempt.outcome != "FAIL"
                || report.verified
                || report.bitwise_parity
                || !attempt.status.is_some_and(|status| status != 0)
                || attempt.signal.is_some()
            {
                return Err("schema 10 ordinary divergence contradicts its process result".into());
            }
            Ok(CellVerdict::ComparedAndDiverged {
                comparison_tier: ComparisonTier::CanonicalBitwise,
                comparison,
                bitwise_parity: false,
                compared_log_messages: counts,
            })
        }
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
    Ordinary,
    Reference {
        candidate: CellIdentity,
    },
    BackendParity {
        reference_backend: String,
        record_envelope: RecordEnvelopePolicy,
    },
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

impl BackendParityCellAttempt {
    pub fn attempt(&self) -> u64 {
        match self {
            Self::Completed { attempt, .. } | Self::UnavailableWithReason { attempt, .. } => {
                *attempt
            }
        }
    }

    pub fn candidate_attempt(&self) -> &ParityAttempt {
        match self {
            Self::Completed {
                candidate_attempt, ..
            }
            | Self::UnavailableWithReason {
                candidate_attempt, ..
            } => candidate_attempt,
        }
    }

    pub fn reference_attempt(&self) -> Option<&ParityAttempt> {
        match self {
            Self::Completed {
                reference_attempt, ..
            } => Some(reference_attempt),
            Self::UnavailableWithReason {
                reference_attempt, ..
            } => match reference_attempt {
                RequiredNullable::Null => None,
                RequiredNullable::Value(attempt) => Some(attempt),
            },
        }
    }

    fn verdicts(
        &self,
        identity: &CellIdentity,
    ) -> Result<
        (
            CellVerdict,
            Option<CellVerdict>,
            ComparisonObservationVerdictV10,
        ),
        String,
    > {
        let candidate = self
            .candidate_attempt()
            .ordinary_verdict(&identity.backend, "1")?;
        let reference = self
            .reference_attempt()
            .map(|attempt| attempt.ordinary_verdict("ptrace", "parity-reference"))
            .transpose()?;
        if let Some(reference) = self.reference_attempt() {
            if reference.0.guest_argv != self.candidate_attempt().0.guest_argv {
                return Err(
                    "schema 10 candidate and reference executed different guest arguments".into(),
                );
            }
        }
        let cross = match self {
            Self::Completed {
                candidate_attempt,
                reference_attempt,
                report,
                ..
            } => {
                if !matches!(candidate, CellVerdict::ComparedAndMatched { .. })
                    || !matches!(reference, Some(CellVerdict::ComparedAndMatched { .. }))
                {
                    return Err(
                        "schema 10 completed parity lacks two eligible ordinary matches".into(),
                    );
                }
                report.validate(&identity.backend)?;
                if candidate_attempt.report()?.as_ref() != Some(&report.candidate.verification)
                    || reference_attempt.report()?.as_ref() != Some(&report.reference.verification)
                {
                    return Err("schema 10 parity operands differ from their exact retained attempt reports".into());
                }
                match report.verdict {
                    BackendParityVerdict::Matched => ComparisonObservationVerdictV10::Matched,
                    BackendParityVerdict::Diverged => ComparisonObservationVerdictV10::Diverged,
                }
            }
            Self::UnavailableWithReason { reason, .. } => {
                if reason.trim().is_empty() {
                    return Err("schema 10 unavailable parity has no retained reason".into());
                }
                ComparisonObservationVerdictV10::UnavailableWithReason {
                    reason: "Cross-backend comparison unavailable; exact detail is retained in the cell artifact".into(),
                }
            }
        };
        Ok((candidate, reference, cross))
    }
}

impl CellBackendParity {
    pub fn from_result_rows(
        identity: &CellIdentity,
        rows: &[(u64, Value)],
    ) -> Result<Self, String> {
        let mut attempts = Vec::new();
        let mut recorded_outcomes = Vec::new();
        for (number, row) in rows {
            let typed: crate::runner::CellResult = serde_json::from_value(row.clone())
                .map_err(|error| format!("schema 10 source result is malformed: {error}"))?;
            typed.require_current_classification()?;
            typed.require_current_timeout_policy()?;
            let actual = CellIdentity {
                lane: typed.lane.clone(),
                category: typed.category.clone(),
                test: typed.test.clone(),
                mode: typed.mode.clone(),
                backend: typed.backend.clone().ok_or("parity cell omitted backend")?,
            };
            if actual != *identity || typed.attempt != *number {
                return Err("schema 10 source cell has different identity or outer attempt".into());
            }
            let raw = row
                .get("attempts")
                .and_then(Value::as_array)
                .ok_or("parity cell omitted attempts")?;
            if raw.is_empty() || raw.len() > 2 {
                return Err("recorded parity cell has no candidate attempt or extra operands; raw result remains retained".into());
            }
            let candidate_attempt = ParityAttempt::from_value(raw[0].clone())?;
            let reference_attempt = raw
                .get(1)
                .cloned()
                .map(ParityAttempt::from_value)
                .transpose()?;
            let attempt = if let Some(report) = typed.backend_parity {
                BackendParityCellAttempt::Completed {
                    attempt: *number,
                    candidate_attempt,
                    reference_attempt: reference_attempt
                        .ok_or("completed parity omitted its reference attempt")?,
                    report,
                }
            } else {
                BackendParityCellAttempt::UnavailableWithReason {
                    attempt: *number,
                    candidate_attempt,
                    reference_attempt: reference_attempt
                        .map_or(RequiredNullable::Null, RequiredNullable::Value),
                    reason: typed
                        .reason
                        .filter(|reason| !reason.trim().is_empty())
                        .ok_or("unavailable parity omitted its reason")?,
                }
            };
            recorded_outcomes.push((*number, typed.outcome));
            attempts.push(attempt);
        }
        let evidence = Self {
            reference_backend: "ptrace".into(),
            record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
            attempts,
        };
        let actual = evidence.outer_outcomes(identity)?;
        if actual
            .iter()
            .map(|(number, outcome)| (*number, *outcome))
            .collect::<Vec<_>>()
            != recorded_outcomes
                .iter()
                .map(|(number, outcome)| (*number, outcome.as_str()))
                .collect::<Vec<_>>()
        {
            return Err(
                "schema 10 retained parity results contradict their actual outer outcomes".into(),
            );
        }
        Ok(evidence)
    }

    pub fn summary(&self, identity: &CellIdentity) -> Result<CellBackendParitySummary, String> {
        self.outer_outcomes(identity)?;
        let digest = |value: Option<&String>| {
            value
                .cloned()
                .map_or(RequiredNullable::Null, RequiredNullable::Value)
        };
        let attempts = self
            .attempts
            .iter()
            .map(|attempt| {
                let (candidate, reference, cross) = attempt.verdicts(identity)?;
                Ok(BackendParityAttemptSummary {
                    attempt: attempt.attempt(),
                    candidate: compact_cell_verdict(&candidate),
                    reference: reference
                        .as_ref()
                        .map(compact_cell_verdict)
                        .map_or(RequiredNullable::Null, RequiredNullable::Value),
                    cross,
                    candidate_verification_report_sha256: digest(
                        attempt
                            .candidate_attempt()
                            .0
                            .verification_report_sha256
                            .as_ref(),
                    ),
                    reference_verification_report_sha256: digest(
                        attempt
                            .reference_attempt()
                            .and_then(|attempt| attempt.0.verification_report_sha256.as_ref()),
                    ),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(CellBackendParitySummary {
            reference_backend: self.reference_backend.clone(),
            record_envelope: self.record_envelope,
            attempts,
        })
    }

    pub fn relation(&self, candidate: CellIdentity) -> BackendParityRelation {
        BackendParityRelation {
            candidate,
            reference_backend: self.reference_backend.clone(),
            record_envelope: self.record_envelope,
        }
    }

    fn outer_outcomes(&self, identity: &CellIdentity) -> Result<Vec<(u64, &'static str)>, String> {
        self.relation(identity.clone()).validate()?;
        let outcomes = self
            .attempts
            .iter()
            .map(|attempt| {
                let (candidate, _, cross) = attempt.verdicts(identity)?;
                let outcome = match cross {
                    ComparisonObservationVerdictV10::Matched => "PASS",
                    ComparisonObservationVerdictV10::Diverged => "FAIL",
                    ComparisonObservationVerdictV10::UnavailableWithReason { .. } => {
                        if matches!(candidate, CellVerdict::ComparedAndDiverged { .. }) {
                            "FAIL"
                        } else {
                            "ERROR"
                        }
                    }
                };
                Ok((attempt.attempt(), outcome))
            })
            .collect::<Result<Vec<_>, String>>()?;
        // Retain the producer's complete, bounded history, including refusal of
        // holes, duplicates, and attempts after a terminal outer PASS.
        crate::runner::outcome_after_retries(outcomes.iter().copied())?;
        Ok(outcomes)
    }

    pub fn candidate_verdict(&self, identity: &CellIdentity) -> Result<CellVerdict, String> {
        let outcomes = self.outer_outcomes(identity)?;
        let selected = crate::runner::outcome_after_retries(outcomes.iter().copied())?;
        let index = outcomes
            .iter()
            .rposition(|(_, outcome)| *outcome == selected)
            .ok_or("schema 10 parity history has no selected terminal outcome")?;
        self.attempts[index]
            .candidate_attempt()
            .ordinary_verdict(&identity.backend, "1")
    }

    pub fn observations(
        &self,
        identity: &CellIdentity,
    ) -> Result<Vec<ComparisonObservationV10>, String> {
        self.outer_outcomes(identity)?;
        let mut observations = Vec::new();
        for attempt in &self.attempts {
            let (candidate, reference, cross) = attempt.verdicts(identity)?;
            observations.push(ComparisonObservationV10 {
                identity: identity.clone(),
                relation: ComparisonRelationV10::Ordinary,
                outer_attempt: Some(attempt.attempt()),
                verdict: observation_verdict(&candidate),
            });
            if let Some(reference) = reference {
                let reference_identity = CellIdentity {
                    backend: "ptrace".into(),
                    ..identity.clone()
                };
                observations.push(ComparisonObservationV10 {
                    identity: reference_identity,
                    relation: ComparisonRelationV10::Reference {
                        candidate: identity.clone(),
                    },
                    outer_attempt: Some(attempt.attempt()),
                    verdict: observation_verdict(&reference),
                });
            }
            observations.push(ComparisonObservationV10 {
                identity: identity.clone(),
                relation: ComparisonRelationV10::BackendParity {
                    reference_backend: self.reference_backend.clone(),
                    record_envelope: self.record_envelope,
                },
                outer_attempt: Some(attempt.attempt()),
                verdict: cross,
            });
        }
        Ok(observations)
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
    pub missing_backend_parity: Vec<BackendParityRelation>,
    pub missing_test_producers: Vec<TestResultProducerSelectionV10>,
    pub full_test_results: bool,
    pub full_backend_parity: bool,
}

fn validate_ordinary_verdict(identity: &CellIdentity, verdict: &CellVerdict) -> Result<(), String> {
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
            if *comparison_tier != ComparisonTier::CanonicalBitwise
                || !comparison.is_canonical_bitwise_info_v1_for_time_policy(
                    expected_time,
                    compared_log_messages,
                )
                || *bitwise_parity != matches!(verdict, CellVerdict::ComparedAndMatched { .. })
            {
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
                .selected_backend_parity
                .windows(2)
                .all(|pair| pair[0] < pair[1])
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
        let mut planned = BTreeMap::new();
        for relation in &self.selected_backend_parity {
            relation.validate()?;
            if !selected.contains(&relation.candidate)
                || planned.insert(&relation.candidate, relation).is_some()
            {
                return Err(
                    "schema 10 parity population has an unselected or duplicate candidate".into(),
                );
            }
        }
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
            match (&cell.backend_parity, planned.get(&identity)) {
                (RequiredNullable::Null, None) => {}
                (RequiredNullable::Value(summary), Some(relation)) => {
                    if summary.reference_backend != relation.reference_backend
                        || summary.record_envelope != relation.record_envelope
                        || summary.attempts.is_empty()
                        || !summary
                            .attempts
                            .windows(2)
                            .all(|pair| pair[0].attempt < pair[1].attempt)
                    {
                        return Err(
                            "schema 10 parity summary differs from its planned relation".into()
                        );
                    }
                    for attempt in &summary.attempts {
                        if attempt.attempt == 0
                            || attempt.attempt > crate::runner::MAX_ATTEMPTS_PER_CELL
                        {
                            return Err(
                                "schema 10 parity summary has an invalid outer attempt".into()
                            );
                        }
                        validate_ordinary_verdict(&identity, &attempt.candidate)?;
                        if attempt.candidate != compact_cell_verdict(&attempt.candidate) {
                            return Err(
                                "schema 10 candidate summary contains a noncanonical reason".into(),
                            );
                        }
                        if let RequiredNullable::Value(reference) = &attempt.reference {
                            validate_ordinary_verdict(
                                &CellIdentity {
                                    backend: "ptrace".into(),
                                    ..identity.clone()
                                },
                                reference,
                            )?;
                            if *reference != compact_cell_verdict(reference) {
                                return Err(
                                    "schema 10 reference summary contains a noncanonical reason"
                                        .into(),
                                );
                            }
                        }
                        for digest in [
                            &attempt.candidate_verification_report_sha256,
                            &attempt.reference_verification_report_sha256,
                        ] {
                            if matches!(digest, RequiredNullable::Value(value) if !is_lower_hex(value, 64))
                            {
                                return Err(
                                    "schema 10 parity summary has a malformed operand digest"
                                        .into(),
                                );
                            }
                        }
                        if let ComparisonObservationVerdictV10::UnavailableWithReason { reason } =
                            &attempt.cross
                        {
                            if reason
                                != "Cross-backend comparison unavailable; exact detail is retained in the cell artifact"
                            {
                                return Err(
                                    "schema 10 cross summary contains a noncanonical reason".into(),
                                );
                            }
                        }
                    }
                }
                _ => {
                    return Err(
                        "schema 10 recorded cell omitted or added a planned parity relation".into(),
                    );
                }
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
        let (planned_cells, planned_parity) = plan.populations()?;
        if evidence.selected != planned_cells || evidence.selected_backend_parity != planned_parity
        {
            return Err(
                "schema 10 cell or parity population differs from the independently retained plan"
                    .into(),
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
        let missing_backend_parity = planned_parity
            .into_iter()
            .filter(|relation| !recorded.contains(&relation.candidate))
            .collect::<Vec<_>>();
        let mut full_backend_parity = missing_backend_parity.is_empty();
        let mut observations = Vec::new();
        for cell in &artifact_cells {
            match &cell.backend_parity {
                RequiredNullable::Value(parity) => {
                    let measured = parity.observations(&cell.identity())?;
                    full_backend_parity &= parity.summary(&cell.identity())?.is_clean_match();
                    observations.extend(measured);
                }
                RequiredNullable::Null => {
                    if !matches!(
                        cell.cell_verdict,
                        CellVerdict::PerformsNoComparisonByDesign { .. }
                    ) {
                        observations.push(ComparisonObservationV10 {
                            identity: cell.identity(),
                            relation: ComparisonRelationV10::Ordinary,
                            outer_attempt: None,
                            verdict: observation_verdict(&cell.cell_verdict),
                        });
                    }
                }
            }
        }
        Ok(Some(VerifiedValidationEvidenceV10 {
            cell_results: evidence,
            test_results,
            observations,
            missing_cells,
            missing_backend_parity,
            missing_test_producers: Vec::new(),
            full_test_results: true,
            full_backend_parity,
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
            let cell: CellArtifactResultV10 = serde_json::from_value(value)
                .map_err(|error| format!("invalid schema 10 full cell evidence: {error}"))?;
            validate_ordinary_verdict(&cell.identity(), &cell.cell_verdict)?;
            cells.push(cell);
        }
        let summaries = cells
            .iter()
            .map(CellArtifactResultV10::summary)
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
