//! Shared serialized type for `stress-series` rows.
//!
//! Hermit owns the artifact consumed by the compatibility scorecard. The
//! parent writer imports this module and checks every proposed row against the
//! same type before appending it, so the Python projection is not a second
//! schema authority.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::canonical_verdict::Verdict;
pub use crate::host_capability::CapabilityVerdict as HostCapabilityVerdict;
pub use crate::host_capability::HostCapabilities;
pub use crate::host_capability::HostCapability;
use crate::runner::FailureClass;
use crate::runner::ObservedResult;

pub const STRESS_SERIES_SCHEMA_V1: &str = "stress-series/v1";
pub const STRESS_SERIES_SCHEMA_V2: &str = "stress-series/v2";
pub const STRESS_SERIES_SCHEMA_V3: &str = "stress-series/v3";
// Frozen when introduced in v2 and retained by v3: extending the machine
// vocabulary must not retroactively make already-written rows unreadable. A
// new capability therefore requires a new stress-series schema before
// producers may emit it.
const STRESS_SERIES_V2_HOST_CAPABILITIES: [HostCapability; 2] =
    [HostCapability::CpuidFaulting, HostCapability::Kvm];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SeriesSchema {
    #[serde(rename = "stress-series/v1")]
    V1,
    #[serde(rename = "stress-series/v2")]
    V2,
    #[serde(rename = "stress-series/v3")]
    V3,
}

impl SeriesSchema {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1 => STRESS_SERIES_SCHEMA_V1,
            Self::V2 => STRESS_SERIES_SCHEMA_V2,
            Self::V3 => STRESS_SERIES_SCHEMA_V3,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum SeriesProducer {
    #[serde(rename = "validate")]
    Validate,
    #[serde(rename = "pressure-test")]
    PressureTest,
    #[serde(rename = "hermit-repeat")]
    HermitRepeat,
}

impl SeriesProducer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Validate => "validate",
            Self::PressureTest => "pressure-test",
            Self::HermitRepeat => "hermit-repeat",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesOutcome {
    Passed,
    Diverged,
    NoResult,
    Timeout,
    Errored,
    Skipped,
}

impl SeriesOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Diverged => "diverged",
            Self::NoResult => "no_result",
            Self::Timeout => "timeout",
            Self::Errored => "errored",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDepth {
    pub commits: u64,
    pub first_parent: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesCoordinates {
    #[serde(default)]
    pub first_divergent_scheduler_turn: Option<u64>,
    #[serde(default)]
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    #[serde(default)]
    pub first_divergent_record: Option<u64>,
    #[serde(default)]
    pub first_divergent_syscall: Option<u64>,
}

impl SeriesCoordinates {
    fn has_position(&self) -> bool {
        self.first_divergent_scheduler_turn.is_some()
            || self.first_divergent_virtual_nanoseconds.is_some()
            || self.first_divergent_record.is_some()
            || self.first_divergent_syscall.is_some()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FirstDivergentMessages {
    #[serde(deserialize_with = "deserialize_nullable_string")]
    pub left: Option<String>,
    #[serde(deserialize_with = "deserialize_nullable_string")]
    pub right: Option<String>,
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesRuntimeMeasurement {
    #[serde(default)]
    pub scheduler_turns: Option<u64>,
    #[serde(default)]
    pub virtual_nanoseconds: Option<u64>,
    #[serde(default)]
    pub syscalls: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesRuntime {
    #[serde(default)]
    pub run1: Option<SeriesRuntimeMeasurement>,
    #[serde(default)]
    pub run2: Option<SeriesRuntimeMeasurement>,
    #[serde(default)]
    pub wall_time_min_ms: Option<f64>,
    #[serde(default)]
    pub wall_time_max_ms: Option<f64>,
}

/// Producer-owned evidence for one inner invocation that did not produce a
/// canonical comparison. The containing series row identifies the outer cell
/// attempt; this retains the typed process disposition used to distinguish a
/// timeout from an unavailable result without inferring from duration or
/// backend.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SeriesNoVerdictKind {
    Unspecified,
    ComparisonRefused,
    NotRun,
    FirstRunRejected,
    InfrastructureError,
    MissingReportTimeout,
    NoncanonicalMatch,
    NoncanonicalDivergence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesAttemptDisposition {
    pub index: String,
    pub kind: SeriesNoVerdictKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub attempt_outcome: String,
    pub disposition: SeriesOutcome,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub status: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub verification_report_sha256: Option<String>,
}

/// Exact source-row identity and its non-comparison dispositions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesNoVerdictEvidence {
    pub evidence_sha256: String,
    pub attempts: Vec<SeriesAttemptDisposition>,
}

/// Complete compact inner history for one pressure CellResult. The digest uses
/// the same normalized source identity as no_verdict_evidence. The projection
/// validates original reports before constructing this record; a digest alone
/// does not authenticate source bytes that a reader does not possess.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesPressureEvidence {
    pub evidence_sha256: String,
    pub attempts: Vec<SeriesPressureAttempt>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesPressureAttempt {
    pub index: String,
    pub outcome: String,
    pub error_kind: Option<String>,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub comparison: Option<SeriesPressureComparison>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesPressureComparison {
    pub verdict: Verdict,
    pub canonical: bool,
    pub report_sha256: String,
    pub no_result_kind: Option<SeriesNoVerdictKind>,
}

impl SeriesPressureAttempt {
    /// Validate one retained invocation without changing its framework outcome.
    /// Callers that have original report bytes must additionally validate their
    /// digest and report semantics before constructing these compact facts.
    pub fn validate_for_mode(&self, mode: &str) -> Result<(), String> {
        let comparison_mode = matches!(mode, "verify" | "replay" | "chaos");
        if !matches!(self.outcome.as_str(), "PASS" | "FAIL" | "ERROR") {
            return Err("pressure_evidence has an unsupported inner outcome".into());
        }
        if self
            .error_kind
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
            || self.status.is_some_and(|status| status < 0)
            || self.signal.is_some_and(|signal| signal <= 0)
            || (self.status.is_some() && self.signal.is_some())
        {
            return Err("pressure_evidence has an invalid process disposition".into());
        }
        let nonzero_process = self.status.is_some_and(|status| status > 0) || self.signal.is_some();
        let completed_pass = self.outcome == "PASS" && !self.timed_out && self.error_kind.is_none();
        if self.outcome == "PASS"
            && (!completed_pass || (self.status.is_none() && self.signal.is_none()))
        {
            return Err(
                "pressure_evidence passing invocation lacks a completed process disposition".into(),
            );
        }
        let Some(comparison) = &self.comparison else {
            let prelaunch_timeout = self.outcome == "ERROR"
                && self.timed_out
                && matches!(
                    self.error_kind.as_deref(),
                    Some("incomplete-verification-evidence" | "cpu-timeout" | "wall-timeout")
                );
            if !comparison_mode
                && self.status.is_none()
                && self.signal.is_none()
                && !prelaunch_timeout
            {
                return Err(
                    "pressure_evidence noncomparison invocation lacks a process disposition".into(),
                );
            }
            if comparison_mode
                && !(self.outcome == "ERROR"
                    && self.timed_out
                    && self.error_kind.is_some()
                    && nonzero_process)
            {
                return Err("pressure_evidence comparison invocation omitted its report without a typed timeout".into());
            }
            return Ok(());
        };
        if !comparison_mode {
            return Err("pressure_evidence comparison appears on a noncomparison mode".into());
        }
        if !is_sha256(&comparison.report_sha256) {
            return Err(
                "pressure_evidence comparison report_sha256 must be lowercase 64-hex".into(),
            );
        }
        if comparison.canonical
            && !matches!(comparison.verdict, Verdict::Matched | Verdict::Diverged)
        {
            return Err("pressure_evidence non-verdict cannot claim a canonical comparison".into());
        }
        if comparison.verdict != Verdict::NoResult && comparison.no_result_kind.is_some() {
            return Err("pressure_evidence comparison carries an unrelated no_result_kind".into());
        }
        let valid = match comparison.verdict {
            Verdict::Matched => {
                completed_pass
                    && self.signal.is_none()
                    && self
                        .status
                        .is_some_and(|status| mode == "chaos" || status == 0)
            }
            Verdict::Diverged => {
                self.outcome == "FAIL"
                    && !self.timed_out
                    && self.error_kind.is_none()
                    && nonzero_process
            }
            Verdict::InfrastructureError => {
                self.outcome == "ERROR" && !self.timed_out && nonzero_process
            }
            Verdict::NoResult => match comparison.no_result_kind {
                Some(SeriesNoVerdictKind::Unspecified) => {
                    self.outcome == "ERROR"
                        && !self.timed_out
                        && self.error_kind.is_some()
                        && nonzero_process
                }
                Some(SeriesNoVerdictKind::ComparisonRefused) => {
                    self.outcome == "ERROR"
                        && !self.timed_out
                        && self.error_kind.as_deref() == Some("incomplete-verification-evidence")
                        && nonzero_process
                }
                Some(SeriesNoVerdictKind::NotRun) => {
                    let prelaunch_timeout = self.timed_out
                        && self.status.is_none()
                        && self.signal.is_none()
                        && matches!(
                            self.error_kind.as_deref(),
                            Some(
                                "incomplete-verification-evidence" | "cpu-timeout" | "wall-timeout"
                            )
                        );
                    self.outcome == "ERROR"
                        && self.error_kind.is_some()
                        && (nonzero_process || prelaunch_timeout)
                }
                Some(SeriesNoVerdictKind::FirstRunRejected) => {
                    self.outcome == "FAIL"
                        && !self.timed_out
                        && self.error_kind.is_none()
                        && self.status.is_some_and(|status| status > 0)
                        && self.signal.is_none()
                }
                _ => false,
            },
        };
        if !valid {
            return Err(format!(
                "pressure_evidence {} report contradicts its inner process disposition",
                comparison.verdict
            ));
        }
        Ok(())
    }
}

fn is_prelaunch_timeout_disposition(disposition: &SeriesAttemptDisposition) -> bool {
    disposition.timed_out
        && disposition.status.is_none()
        && disposition.signal.is_none()
        && matches!(
            disposition.error_kind.as_deref(),
            Some("incomplete-verification-evidence" | "cpu-timeout" | "wall-timeout")
        )
}

fn one_run() -> u64 {
    1
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesPayload {
    pub cell: String,
    pub tree: String,
    #[serde(default)]
    pub detcore_tree: Option<String>,
    pub outcome: SeriesOutcome,
    /// The exact framework-written result. Required by `stress-series/v3`;
    /// absent only on retained v1/v2 rows.
    #[serde(default)]
    pub result: Option<ObservedResult>,
    /// Attribution for a non-pass. Required by `stress-series/v3` whenever the
    /// exact result is not `pass`; absent only for passes and retained v1/v2
    /// rows.
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    /// Present on newly emitted comparison-mode rows that produced no complete
    /// canonical product verdict. Historical v3 rows predate this optional
    /// evidence and remain readable, but consumers must not infer the missing
    /// facts from duration, backend, or exit status alone.
    #[serde(default)]
    pub no_verdict_evidence: Option<SeriesNoVerdictEvidence>,
    /// Additive pressure history. Old rows remain readable, but its absence
    /// cannot establish a clean first attempt for sample promotion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_evidence: Option<SeriesPressureEvidence>,
    pub run_index: u64,
    #[serde(default)]
    pub attempt: Option<u64>,
    #[serde(default = "one_run")]
    pub num_runs: u64,
    #[serde(default)]
    pub last_run_index: Option<u64>,
    #[serde(default)]
    pub main_ancestry: Option<bool>,
    #[serde(default)]
    pub runtime: Option<SeriesRuntime>,
    #[serde(default)]
    pub source_tree_dirty: bool,
    #[serde(default)]
    pub depth: BTreeMap<String, SourceDepth>,
    #[serde(default)]
    pub coordinates: Option<SeriesCoordinates>,
    #[serde(default)]
    pub first_divergent_messages: Option<FirstDivergentMessages>,
    /// Required by `stress-series/v2`. This is the measurement authority; the
    /// parent shard path is only a storage index derived from the envelope host.
    /// Absent only on retained v1 rows.
    #[serde(default)]
    pub machine_shortname: Option<String>,
    /// Required by `stress-series/v2`. Absent only on retained v1 rows.
    #[serde(default)]
    pub kernel_version: Option<String>,
    /// Required by `stress-series/v2`. This is the complete set of capability
    /// verdicts that determined which population could execute. It is not
    /// derivable from the machine name or kernel version.
    #[serde(default)]
    pub host_capabilities: Option<HostCapabilities>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SeriesRow {
    /// Filled by readers after deserialization; never part of the artifact.
    #[serde(skip)]
    pub source: String,
    pub schema: SeriesSchema,
    pub event_id: String,
    pub event_type: String,
    pub emitted_at: String,
    pub team: String,
    pub host: String,
    pub producer: SeriesProducer,
    pub run_id: String,
    pub series: SeriesPayload,
}

impl SeriesRow {
    pub fn cell(&self) -> &str {
        &self.series.cell
    }

    pub fn label(&self) -> String {
        format!(
            "{}:{}: {} from {} run {} repetition {} @ {}",
            self.source,
            self.event_id,
            self.series.cell,
            self.producer.as_str(),
            self.run_id,
            self.series.run_index,
            self.series.tree
        )
    }

    /// Validate a newly written row. Historical v1/v2 rows remain
    /// deserializable, but no new row may omit the framework's exact result and
    /// attribution.
    pub fn validate_for_write(&self) -> Result<(), String> {
        if self.schema != SeriesSchema::V3 {
            return Err(format!(
                "new rows must use {STRESS_SERIES_SCHEMA_V3}, got {}",
                self.schema.as_str()
            ));
        }
        self.validate_common()?;
        self.validate_host_facts()?;
        self.validate_classification()?;
        self.require_current_no_verdict_evidence()
    }

    /// Validate a stored row before a reader uses its contents.
    ///
    /// Retained v1/v2 rows remain readable. Every v2/v3 row must carry the
    /// complete host facts that its schema promises, and every v3 row must
    /// carry the exact result classification.
    pub fn validate_for_read(&self) -> Result<(), String> {
        self.validate_common()?;
        if matches!(self.schema, SeriesSchema::V2 | SeriesSchema::V3) {
            self.validate_host_facts()?;
        }
        if self.schema == SeriesSchema::V3 {
            self.validate_classification()?;
        }
        Ok(())
    }

    /// Validate a row before treating it as measurement evidence.
    ///
    /// Retained v1 rows still parse and can be reported, but they cannot safely
    /// compare runs across the same machine name after a kernel change.
    pub fn validate_for_projection(&self) -> Result<(), String> {
        self.validate_common()?;
        if self.schema == SeriesSchema::V1 {
            return Err(format!(
                "{} does not record machine_shortname, kernel_version, and host_capabilities",
                self.schema.as_str()
            ));
        }
        self.validate_host_facts()?;
        if self.schema == SeriesSchema::V3 {
            self.validate_classification()?;
        }
        if self.series.source_tree_dirty {
            return Err(
                "source_tree_dirty is true; dirty source is not checked-in evidence".into(),
            );
        }
        Ok(())
    }

    fn validate_common(&self) -> Result<(), String> {
        for (name, value) in [
            ("event_id", self.event_id.as_str()),
            ("emitted_at", self.emitted_at.as_str()),
            ("host", self.host.as_str()),
            ("run_id", self.run_id.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(format!("{name} must be nonempty"));
            }
        }
        if self.event_type != "series.observation" {
            return Err(format!(
                "event_type must be series.observation, got {:?}",
                self.event_type
            ));
        }
        if self.team != "hermit" {
            return Err(format!("team must be hermit, got {:?}", self.team));
        }
        if !valid_cell(&self.series.cell) {
            return Err(format!(
                "cell must be '<test>/<mode>/<backend>' with the test's own slashes allowed, got {:?}",
                self.series.cell
            ));
        }
        if !is_object_id(&self.series.tree) {
            return Err(format!(
                "tree must be a 40-hex commit sha, got {:?}",
                self.series.tree
            ));
        }
        if let Some(tree) = &self.series.detcore_tree {
            if !is_object_id(tree) {
                return Err(format!(
                    "detcore_tree must be a 40-hex object id when present, got {tree:?}"
                ));
            }
        }
        if self.series.attempt == Some(0) {
            return Err("attempt must be a positive int when present".into());
        }
        if self.series.num_runs == 0 {
            return Err("num_runs must be a positive int".into());
        }
        if let Some(last) = self.series.last_run_index {
            if last < self.series.run_index {
                return Err("last_run_index must be at least run_index".into());
            }
            if last - self.series.run_index + 1 < self.series.num_runs {
                return Err(
                    "last_run_index span must contain at least num_runs observations".into(),
                );
            }
        }
        if let Some(runtime) = &self.series.runtime {
            for (name, value) in [
                ("wall_time_min_ms", runtime.wall_time_min_ms),
                ("wall_time_max_ms", runtime.wall_time_max_ms),
            ] {
                if value.is_some_and(|value| !value.is_finite() || value < 0.0) {
                    return Err(format!(
                        "runtime {name} must be a non-negative number or null"
                    ));
                }
            }
            if let (Some(minimum), Some(maximum)) =
                (runtime.wall_time_min_ms, runtime.wall_time_max_ms)
            {
                if minimum > maximum {
                    return Err("runtime wall_time_min_ms must not exceed wall_time_max_ms".into());
                }
            }
        }
        for (repository, depth) in &self.series.depth {
            if repository.is_empty() {
                return Err("depth repository keys must be nonempty strings".into());
            }
            if depth.commits == 0 || depth.first_parent == 0 {
                return Err(format!(
                    "depth {repository}.commits and first_parent must be positive"
                ));
            }
        }
        if self.series.outcome != SeriesOutcome::Diverged
            && (self
                .series
                .coordinates
                .as_ref()
                .is_some_and(SeriesCoordinates::has_position)
                || self.series.first_divergent_messages.is_some())
        {
            return Err(format!(
                "outcome {:?} must not carry divergence evidence; only diverged may",
                self.series.outcome.as_str()
            ));
        }
        if let Some(messages) = &self.series.first_divergent_messages {
            for (side, value) in [("left", &messages.left), ("right", &messages.right)] {
                if value.as_ref().is_some_and(|value| value.is_empty()) {
                    return Err(format!(
                        "first_divergent_messages.{side} must be a nonempty string or null"
                    ));
                }
            }
            if messages.left.is_none() && messages.right.is_none() {
                return Err("first_divergent_messages must contain at least one message".into());
            }
        }
        if let Some(evidence) = &self.series.no_verdict_evidence {
            self.validate_no_verdict_evidence(evidence)?;
        }
        if let Some(evidence) = &self.series.pressure_evidence {
            self.validate_pressure_evidence(evidence)?;
        }
        Ok(())
    }

    fn validate_pressure_evidence(&self, evidence: &SeriesPressureEvidence) -> Result<(), String> {
        if self.schema != SeriesSchema::V3 || self.producer != SeriesProducer::PressureTest {
            return Err("pressure_evidence requires a pressure-test stress-series/v3 row".into());
        }
        if self.series.attempt.is_none_or(|attempt| attempt == 0)
            || self.series.num_runs != 1
            || self.series.last_run_index.is_some()
        {
            return Err(
                "pressure_evidence requires one uncompressed explicit positive outer attempt"
                    .into(),
            );
        }
        if !is_sha256(&evidence.evidence_sha256) {
            return Err("pressure_evidence.evidence_sha256 must be lowercase 64-hex".into());
        }
        if self
            .series
            .no_verdict_evidence
            .as_ref()
            .is_some_and(|other| other.evidence_sha256 != evidence.evidence_sha256)
        {
            return Err(
                "pressure_evidence and no_verdict_evidence identify different CellResults".into(),
            );
        }
        if evidence.attempts.is_empty() {
            return Err("pressure_evidence.attempts must retain a nonempty inner history".into());
        }
        let mode = self.series.cell.rsplit('/').nth(1).unwrap_or_default();
        let mut indices = std::collections::BTreeSet::new();
        for attempt in &evidence.attempts {
            if attempt.index.trim().is_empty() || !indices.insert(&attempt.index) {
                return Err("pressure_evidence attempt indices must be nonempty and unique".into());
            }
            attempt.validate_for_mode(mode)?;
        }
        if let Some(other) = &self.series.no_verdict_evidence {
            for disposition in &other.attempts {
                let retained = evidence
                    .attempts
                    .iter()
                    .find(|attempt| attempt.index == disposition.index)
                    .ok_or("pressure_evidence omitted a no_verdict_evidence invocation")?;
                let same_comparison = match (disposition.kind, &retained.comparison) {
                    (SeriesNoVerdictKind::MissingReportTimeout, None) => true,
                    (kind, Some(comparison)) => {
                        let (verdict, no_result_kind) = match kind {
                            SeriesNoVerdictKind::Unspecified | SeriesNoVerdictKind::ComparisonRefused
                            | SeriesNoVerdictKind::NotRun | SeriesNoVerdictKind::FirstRunRejected =>
                                (Verdict::NoResult, Some(kind)),
                            SeriesNoVerdictKind::InfrastructureError => (Verdict::InfrastructureError, None),
                            SeriesNoVerdictKind::NoncanonicalMatch => (Verdict::Matched, None),
                            SeriesNoVerdictKind::NoncanonicalDivergence => (Verdict::Diverged, None),
                            SeriesNoVerdictKind::MissingReportTimeout =>
                                return Err("pressure_evidence supplied a report for a missing-report disposition".into()),
                        };
                        !comparison.canonical
                            && comparison.verdict == verdict
                            && comparison.no_result_kind == no_result_kind
                            && Some(&comparison.report_sha256)
                                == disposition.verification_report_sha256.as_ref()
                    }
                    _ => false,
                };
                if retained.outcome != disposition.attempt_outcome
                    || retained.error_kind != disposition.error_kind
                    || retained.status != disposition.status
                    || retained.signal != disposition.signal
                    || retained.timed_out != disposition.timed_out
                    || !same_comparison
                {
                    return Err(
                        "pressure_evidence contradicts the same no_verdict_evidence invocation"
                            .into(),
                    );
                }
            }
        }
        // Inner history explains qualification; it never rewrites the exact
        // framework result, including a retained PASS with an adverse subrun.
        Ok(())
    }

    fn validate_no_verdict_evidence(
        &self,
        evidence: &SeriesNoVerdictEvidence,
    ) -> Result<(), String> {
        if !is_sha256(&evidence.evidence_sha256) {
            return Err(format!(
                "no_verdict_evidence.evidence_sha256 must be lowercase 64-hex, got {:?}",
                evidence.evidence_sha256
            ));
        }
        if self.schema != SeriesSchema::V3 {
            return Err("no_verdict_evidence is supported only by stress-series/v3".into());
        }
        let attempt = self
            .series
            .attempt
            .ok_or("no_verdict_evidence requires an explicit outer attempt")?;
        let mode = self.series.cell.rsplit('/').nth(1).unwrap_or_default();
        if !matches!(mode, "verify" | "replay" | "chaos") {
            return Err("no_verdict_evidence is valid only for comparison modes".into());
        }
        if attempt == 0 || self.series.num_runs != 1 || self.series.last_run_index.is_some() {
            return Err(
                "no_verdict_evidence must identify one uncollapsed positive outer attempt".into(),
            );
        }
        if self.producer == SeriesProducer::Validate && attempt != self.series.run_index {
            return Err("validate no_verdict_evidence outer attempt must equal run_index".into());
        }
        if matches!(
            self.series.outcome,
            SeriesOutcome::Passed | SeriesOutcome::Diverged | SeriesOutcome::Skipped
        ) {
            return Err(format!(
                "outcome {} cannot carry no_verdict_evidence",
                self.series.outcome.as_str()
            ));
        }
        if evidence.attempts.is_empty() {
            return Err("no_verdict_evidence.attempts must be nonempty".into());
        }
        let mut indices = std::collections::BTreeSet::new();
        let mut saw_timeout = false;
        for disposition in &evidence.attempts {
            if disposition.index.trim().is_empty() || !indices.insert(&disposition.index) {
                return Err(
                    "no_verdict_evidence attempt indices must be nonempty and unique".into(),
                );
            }
            if disposition
                .detail
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err("no_verdict_evidence detail must be nonempty when present".into());
            }
            if disposition.detail.is_some()
                && disposition.kind != SeriesNoVerdictKind::ComparisonRefused
            {
                return Err("only comparison_refused evidence may carry detail".into());
            }
            if disposition
                .error_kind
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                return Err("no_verdict_evidence error_kind must be nonempty when present".into());
            }
            if disposition.status.is_some_and(|status| status < 0)
                || disposition.signal.is_some_and(|signal| signal <= 0)
                || (disposition.status.is_some() && disposition.signal.is_some())
            {
                return Err("no_verdict_evidence status/signal disposition is invalid".into());
            }
            if disposition
                .verification_report_sha256
                .as_ref()
                .is_some_and(|sha| !is_sha256(sha))
            {
                return Err(
                    "no_verdict_evidence verification_report_sha256 must be lowercase 64-hex"
                        .into(),
                );
            }
            let has_nonzero_process_disposition = matches!(
                (disposition.status, disposition.signal),
                (Some(status), None) if status > 0
            ) || matches!(
                (disposition.status, disposition.signal),
                (None, Some(signal)) if signal > 0
            );
            match disposition.kind {
                SeriesNoVerdictKind::Unspecified => {
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition
                            .error_kind
                            .as_ref()
                            .is_none_or(|value| value.trim().is_empty())
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "unspecified evidence must carry attempt outcome ERROR, an error_kind, a verification report, exactly one nonzero status or signal, timed_out=false, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::ComparisonRefused => {
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition
                            .detail
                            .as_ref()
                            .is_none_or(|value| value.trim().is_empty())
                        || disposition.timed_out
                        || disposition.error_kind.as_deref()
                            != Some("incomplete-verification-evidence")
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "comparison_refused evidence must carry nonempty detail, attempt outcome ERROR, error_kind incomplete-verification-evidence, a verification report, exactly one nonzero status or signal, timed_out=false, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::NotRun => {
                    let expected = if disposition.timed_out {
                        SeriesOutcome::Timeout
                    } else {
                        SeriesOutcome::NoResult
                    };
                    let no_process_timeout = is_prelaunch_timeout_disposition(disposition);
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != expected
                        || disposition
                            .error_kind
                            .as_ref()
                            .is_none_or(|value| value.trim().is_empty())
                        || !(has_nonzero_process_disposition || no_process_timeout)
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "not_run evidence must carry attempt outcome ERROR, an error_kind, a verification report, either one nonzero process disposition or an explicit pre-launch timeout, and a matching typed disposition"
                                .into(),
                        );
                    }
                    saw_timeout |= disposition.timed_out;
                }
                SeriesNoVerdictKind::FirstRunRejected => {
                    if disposition.attempt_outcome != "FAIL"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition.error_kind.is_some()
                        || disposition.status.is_none_or(|status| status <= 0)
                        || disposition.signal.is_some()
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "first_run_rejected evidence must carry attempt outcome FAIL, no error_kind, a nonzero status without signal, timed_out=false, a verification report, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::NoncanonicalMatch => {
                    let status_is_valid = disposition
                        .status
                        .is_some_and(|status| status >= 0 && (mode == "chaos" || status == 0));
                    if disposition.attempt_outcome != "PASS"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition.error_kind.is_some()
                        || !status_is_valid
                        || disposition.signal.is_some()
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "noncanonical_match evidence must carry attempt outcome PASS, no error_kind, a valid status without signal, timed_out=false, a verification report, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::NoncanonicalDivergence => {
                    if disposition.attempt_outcome != "FAIL"
                        || disposition.disposition != SeriesOutcome::NoResult
                        || disposition.timed_out
                        || disposition.error_kind.is_some()
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "noncanonical_divergence evidence must carry attempt outcome FAIL, no error_kind, exactly one nonzero status or signal, timed_out=false, a verification report, and no_result disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::InfrastructureError => {
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != SeriesOutcome::Errored
                        || disposition.timed_out
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_none()
                    {
                        return Err(
                            "infrastructure_error evidence must carry attempt outcome ERROR, a verification report, exactly one nonzero status or signal, timed_out=false, and errored disposition"
                                .into(),
                        );
                    }
                }
                SeriesNoVerdictKind::MissingReportTimeout => {
                    if disposition.attempt_outcome != "ERROR"
                        || disposition.disposition != SeriesOutcome::Timeout
                        || !disposition.timed_out
                        || disposition
                            .error_kind
                            .as_ref()
                            .is_none_or(|value| value.trim().is_empty())
                        || !has_nonzero_process_disposition
                        || disposition.verification_report_sha256.is_some()
                    {
                        return Err(
                            "missing_report_timeout evidence must carry attempt outcome ERROR, an error_kind, no verification report, exactly one nonzero status or signal, timed_out=true, and timeout disposition"
                                .into(),
                        );
                    }
                    saw_timeout = true;
                }
            }
        }
        if self.series.result == Some(ObservedResult::Timeout) && !saw_timeout {
            return Err("timeout result has no timed-out attempt disposition".into());
        }
        if saw_timeout
            && self.series.result.is_some()
            && self.series.result != Some(ObservedResult::Timeout)
        {
            return Err(format!(
                "timed-out attempt disposition contradicts result {:?}",
                self.series.result
            ));
        }
        Ok(())
    }

    fn require_current_no_verdict_evidence(&self) -> Result<(), String> {
        let mode = self.series.cell.rsplit('/').nth(1).unwrap_or_default();
        let requires_evidence = matches!(
            self.series.outcome,
            SeriesOutcome::NoResult | SeriesOutcome::Timeout | SeriesOutcome::Errored
        );
        if matches!(mode, "verify" | "replay" | "chaos")
            && requires_evidence
            && self.series.no_verdict_evidence.is_none()
        {
            return Err(format!(
                "new comparison-mode {} row must carry no_verdict_evidence",
                self.series.outcome.as_str()
            ));
        }
        Ok(())
    }

    fn validate_host_facts(&self) -> Result<(), String> {
        let machine = self
            .series
            .machine_shortname
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or("series missing machine_shortname")?;
        if machine.contains('/') || machine.contains('.') {
            return Err(format!(
                "machine_shortname must be a short hostname, got {machine:?}"
            ));
        }
        if self.host != machine {
            return Err(format!(
                "envelope host {:?} does not match machine_shortname {machine:?}",
                self.host
            ));
        }
        self.series
            .kernel_version
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "series missing kernel_version".to_string())?;
        let capabilities = self
            .series
            .host_capabilities
            .as_ref()
            .ok_or_else(|| "series missing host_capabilities".to_string())?;
        for capability in STRESS_SERIES_V2_HOST_CAPABILITIES {
            let verdict = capabilities.get(&capability).ok_or_else(|| {
                format!(
                    "host_capabilities missing required capability {:?}",
                    capability.value()
                )
            })?;
            if verdict.evidence.trim().is_empty() {
                return Err(format!(
                    "host_capabilities.{:?}.evidence must be nonempty",
                    capability.value()
                ));
            }
        }
        if capabilities.len() != STRESS_SERIES_V2_HOST_CAPABILITIES.len() {
            return Err("host_capabilities must contain the complete closed capability set".into());
        }
        Ok(())
    }

    fn validate_classification(&self) -> Result<(), String> {
        let valid = matches!(
            (
                self.series.outcome,
                self.series.result,
                self.series.failure_class,
            ),
            (SeriesOutcome::Passed, Some(ObservedResult::Pass), None)
                | (
                    SeriesOutcome::Diverged,
                    Some(
                        ObservedResult::DeterminismFailure
                            | ObservedResult::ParityFailure
                            | ObservedResult::ReplayFailure
                    ),
                    Some(FailureClass::ProductFailure)
                )
                | (
                    SeriesOutcome::Errored,
                    Some(ObservedResult::CrashError),
                    Some(FailureClass::ProductFailure)
                )
                | (
                    SeriesOutcome::Timeout,
                    Some(ObservedResult::Timeout),
                    Some(FailureClass::NoResult)
                )
                | (
                    SeriesOutcome::NoResult,
                    Some(ObservedResult::Oom),
                    Some(FailureClass::NoResult)
                )
                | (
                    SeriesOutcome::Errored,
                    Some(ObservedResult::SandboxDenied | ObservedResult::InfrastructureError)
                        | None,
                    Some(FailureClass::UnderstoodInfrastructureFailure)
                )
                | (
                    SeriesOutcome::Skipped,
                    None,
                    Some(FailureClass::UnderstoodPrerequisiteFailure)
                )
                | (SeriesOutcome::NoResult, None, Some(FailureClass::NoResult))
        );
        if valid {
            Ok(())
        } else {
            Err(format!(
                "stress-series/v3 classification mismatch: outcome={} result={:?} failure_class={:?}",
                self.series.outcome.as_str(),
                self.series.result,
                self.series.failure_class
            ))
        }
    }
}

fn is_object_id(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_cell(value: &str) -> bool {
    let mut parts = value.rsplitn(3, '/');
    let backend = parts.next().unwrap_or_default();
    let mode = parts.next().unwrap_or_default();
    let test = parts.next().unwrap_or_default();
    !test.is_empty()
        && !mode.is_empty()
        && !backend.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(schema: SeriesSchema) -> SeriesRow {
        SeriesRow {
            source: String::new(),
            schema,
            event_id: "event".into(),
            event_type: "series.observation".into(),
            emitted_at: "2026-08-27T00:00:00Z".into(),
            team: "hermit".into(),
            host: "fixture-host".into(),
            producer: SeriesProducer::Validate,
            run_id: "run".into(),
            series: SeriesPayload {
                cell: "fixture/test/verify/ptrace".into(),
                tree: "a".repeat(40),
                detcore_tree: None,
                outcome: SeriesOutcome::Passed,
                result: Some(ObservedResult::Pass),
                failure_class: None,
                no_verdict_evidence: None,
                pressure_evidence: None,
                run_index: 1,
                attempt: None,
                num_runs: 1,
                last_run_index: None,
                main_ancestry: Some(true),
                runtime: None,
                source_tree_dirty: false,
                depth: BTreeMap::new(),
                coordinates: None,
                first_divergent_messages: None,
                machine_shortname: Some("fixture-host".into()),
                kernel_version: Some("7.1.3-test".into()),
                host_capabilities: Some(BTreeMap::from([
                    (
                        HostCapability::CpuidFaulting,
                        HostCapabilityVerdict {
                            present: true,
                            evidence: "fixture cpuid probe".into(),
                        },
                    ),
                    (
                        HostCapability::Kvm,
                        HostCapabilityVerdict {
                            present: false,
                            evidence: "fixture kvm probe".into(),
                        },
                    ),
                ])),
            },
        }
    }

    fn no_verdict_row() -> SeriesRow {
        let mut fixture = row(SeriesSchema::V3);
        fixture.series.outcome = SeriesOutcome::NoResult;
        fixture.series.result = None;
        fixture.series.failure_class = Some(FailureClass::NoResult);
        fixture.series.attempt = Some(1);
        fixture.series.no_verdict_evidence = Some(SeriesNoVerdictEvidence {
            evidence_sha256: "b".repeat(64),
            attempts: vec![SeriesAttemptDisposition {
                index: "1".into(),
                kind: SeriesNoVerdictKind::NotRun,
                detail: None,
                attempt_outcome: "ERROR".into(),
                disposition: SeriesOutcome::NoResult,
                error_kind: Some("incomplete-verification-evidence".into()),
                status: Some(125),
                signal: None,
                timed_out: false,
                verification_report_sha256: Some("c".repeat(64)),
            }],
        });
        fixture
    }

    #[test]
    fn v3_requires_matching_machine_and_kernel() {
        let mut fixture = row(SeriesSchema::V3);
        fixture.validate_for_write().unwrap();
        fixture.series.kernel_version = None;
        assert_eq!(
            fixture.validate_for_write().unwrap_err(),
            "series missing kernel_version"
        );
        fixture.series.kernel_version = Some("7.1.3-test".into());
        fixture.series.machine_shortname = Some("other-host".into());
        assert!(
            fixture
                .validate_for_write()
                .unwrap_err()
                .contains("does not match machine_shortname")
        );
    }

    #[test]
    fn v1_is_readable_but_not_new_measurement_evidence() {
        let mut fixture = row(SeriesSchema::V1);
        fixture.series.machine_shortname = None;
        fixture.series.kernel_version = None;
        fixture.series.host_capabilities = None;
        let encoded = serde_json::to_string(&fixture).unwrap();
        let decoded: SeriesRow = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.schema, SeriesSchema::V1);
        assert!(decoded.validate_for_write().is_err());
        assert!(decoded.validate_for_projection().is_err());
    }

    #[test]
    fn v3_requires_every_capability_verdict_with_evidence() {
        let mut fixture = row(SeriesSchema::V3);
        fixture.series.host_capabilities = None;
        assert_eq!(
            fixture.validate_for_write().unwrap_err(),
            "series missing host_capabilities"
        );

        for capability in STRESS_SERIES_V2_HOST_CAPABILITIES {
            let mut fixture = row(SeriesSchema::V3);
            fixture
                .series
                .host_capabilities
                .as_mut()
                .unwrap()
                .remove(&capability);
            assert!(
                fixture
                    .validate_for_write()
                    .unwrap_err()
                    .contains(capability.value()),
                "missing {} was not named",
                capability.value()
            );
        }

        let mut fixture = row(SeriesSchema::V3);
        fixture
            .series
            .host_capabilities
            .as_mut()
            .unwrap()
            .get_mut(&HostCapability::Kvm)
            .unwrap()
            .evidence
            .clear();
        assert!(fixture.validate_for_write().unwrap_err().contains("kvm"));
    }

    #[test]
    fn read_validation_keeps_v1_readable_and_refuses_incomplete_v2_by_name() {
        let mut legacy = row(SeriesSchema::V1);
        legacy.series.machine_shortname = None;
        legacy.series.kernel_version = None;
        legacy.series.host_capabilities = None;
        legacy.validate_for_read().unwrap();

        for schema in [SeriesSchema::V2, SeriesSchema::V3] {
            let mut current = row(schema);
            current.series.host_capabilities = None;
            assert_eq!(
                current.validate_for_read().unwrap_err(),
                "series missing host_capabilities"
            );

            let mut current = row(schema);
            current
                .series
                .host_capabilities
                .as_mut()
                .unwrap()
                .remove(&HostCapability::Kvm);
            assert!(current.validate_for_read().unwrap_err().contains("kvm"));
        }
    }

    #[test]
    fn non_diverged_rows_cannot_carry_divergence_evidence() {
        let mut fixture = row(SeriesSchema::V3);
        fixture.series.coordinates = Some(SeriesCoordinates {
            first_divergent_record: Some(9),
            ..SeriesCoordinates::default()
        });
        assert!(
            fixture
                .validate_for_write()
                .unwrap_err()
                .contains("must not carry divergence evidence")
        );
    }

    #[test]
    fn v2_refuses_error_and_accepts_errored() {
        let mut value = serde_json::to_value(row(SeriesSchema::V2)).unwrap();
        value["series"]["outcome"] = serde_json::json!("errored");
        let accepted: SeriesRow = serde_json::from_value(value.clone())
            .expect("errored remains a supported non-verdict outcome");
        accepted.validate_for_read().unwrap();

        value["series"]["outcome"] = serde_json::json!("error");
        let error = serde_json::from_value::<SeriesRow>(value)
            .expect_err("schema-v2 must refuse the unsupported error spelling");
        assert!(error.to_string().contains("unknown variant `error`"));
    }

    #[test]
    fn v3_refuses_missing_or_mismatched_classification_by_name() {
        let mut missing_result = row(SeriesSchema::V3);
        missing_result.series.result = None;
        for error in [
            missing_result.validate_for_read().unwrap_err(),
            missing_result.validate_for_write().unwrap_err(),
        ] {
            assert!(error.contains("result=None"));
        }

        let mut missing_class = row(SeriesSchema::V3);
        missing_class.series.outcome = SeriesOutcome::Diverged;
        missing_class.series.result = Some(ObservedResult::DeterminismFailure);
        assert!(
            missing_class
                .validate_for_write()
                .unwrap_err()
                .contains("failure_class=None")
        );

        let mut wrong_result = row(SeriesSchema::V3);
        wrong_result.series.outcome = SeriesOutcome::Diverged;
        wrong_result.series.result = Some(ObservedResult::CrashError);
        wrong_result.series.failure_class = Some(FailureClass::ProductFailure);
        assert!(
            wrong_result
                .validate_for_write()
                .unwrap_err()
                .contains("result=Some(CrashError)")
        );

        let mut retained_v2 = row(SeriesSchema::V2);
        retained_v2.series.result = None;
        retained_v2.series.failure_class = None;
        retained_v2.validate_for_read().unwrap();
        retained_v2.validate_for_projection().unwrap();
        assert_eq!(
            retained_v2.validate_for_write().unwrap_err(),
            "new rows must use stress-series/v3, got stress-series/v2"
        );
    }

    #[test]
    fn current_no_verdict_rows_require_exact_uncollapsed_evidence() {
        let fixture = no_verdict_row();
        fixture.validate_for_read().unwrap();
        fixture.validate_for_write().unwrap();

        let mut legacy = fixture.clone();
        legacy.series.no_verdict_evidence = None;
        legacy.validate_for_read().unwrap();
        assert!(
            legacy
                .validate_for_write()
                .unwrap_err()
                .contains("must carry no_verdict_evidence")
        );

        let mut collapsed = fixture.clone();
        collapsed.series.num_runs = 2;
        collapsed.series.last_run_index = Some(2);
        assert!(
            collapsed
                .validate_for_read()
                .unwrap_err()
                .contains("one uncollapsed positive outer attempt")
        );

        let mut missing_attempt = fixture;
        missing_attempt.series.attempt = None;
        assert!(
            missing_attempt
                .validate_for_read()
                .unwrap_err()
                .contains("explicit outer attempt")
        );

        let mut mismatched_attempt = no_verdict_row();
        mismatched_attempt.series.attempt = Some(2);
        assert!(
            mismatched_attempt
                .validate_for_read()
                .unwrap_err()
                .contains("outer attempt must equal run_index")
        );

        let mut v2 = no_verdict_row();
        v2.schema = SeriesSchema::V2;
        assert!(
            v2.validate_for_read()
                .unwrap_err()
                .contains("supported only by stress-series/v3")
        );
    }

    #[test]
    fn current_prelaunch_timeouts_remain_failed_typed_evidence() {
        for error_kind in ["cpu-timeout", "wall-timeout"] {
            let mut fixture = no_verdict_row();
            fixture.series.outcome = SeriesOutcome::Timeout;
            fixture.series.result = Some(ObservedResult::Timeout);
            let disposition = &mut fixture
                .series
                .no_verdict_evidence
                .as_mut()
                .unwrap()
                .attempts[0];
            disposition.error_kind = Some(error_kind.into());
            disposition.disposition = SeriesOutcome::Timeout;
            disposition.status = None;
            disposition.signal = None;
            disposition.timed_out = true;
            for mode in ["verify", "replay", "chaos"] {
                fixture.series.cell = format!("fixture/test/{mode}/ptrace");
                fixture.validate_for_read().unwrap_or_else(|error| {
                    panic!("current {mode} prelaunch {error_kind} was refused: {error}")
                });
                fixture.validate_for_write().unwrap();
                fixture.validate_for_projection().unwrap();
            }

            for mutation in [
                "missing error kind",
                "unrelated error kind",
                "empty error kind",
                "not timed out",
                "successful attempt",
                "non-timeout disposition",
                "zero status",
                "zero signal",
                "status and signal",
                "missing report",
                "invalid report hash",
                "successful series",
                "missing exact result",
                "product failure classification",
            ] {
                let mut invalid = fixture.clone();
                let disposition = &mut invalid
                    .series
                    .no_verdict_evidence
                    .as_mut()
                    .unwrap()
                    .attempts[0];
                match mutation {
                    "missing error kind" => disposition.error_kind = None,
                    "unrelated error kind" => {
                        disposition.error_kind = Some("infrastructure".into())
                    }
                    "empty error kind" => disposition.error_kind = Some(String::new()),
                    "not timed out" => disposition.timed_out = false,
                    "successful attempt" => disposition.attempt_outcome = "PASS".into(),
                    "non-timeout disposition" => disposition.disposition = SeriesOutcome::NoResult,
                    "zero status" => disposition.status = Some(0),
                    "zero signal" => disposition.signal = Some(0),
                    "status and signal" => {
                        disposition.status = Some(1);
                        disposition.signal = Some(15);
                    }
                    "missing report" => disposition.verification_report_sha256 = None,
                    "invalid report hash" => {
                        disposition.verification_report_sha256 = Some("bad".into())
                    }
                    "successful series" => {
                        invalid.series.outcome = SeriesOutcome::Passed;
                        invalid.series.result = Some(ObservedResult::Pass);
                        invalid.series.failure_class = None;
                    }
                    "missing exact result" => invalid.series.result = None,
                    "product failure classification" => {
                        invalid.series.failure_class = Some(FailureClass::ProductFailure);
                    }
                    _ => unreachable!(),
                }
                assert!(
                    invalid.validate_for_read().is_err(),
                    "{error_kind}: accepted {mutation} on read"
                );
                assert!(
                    invalid.validate_for_write().is_err(),
                    "{error_kind}: accepted {mutation} on write"
                );
                assert!(
                    invalid.validate_for_projection().is_err(),
                    "{error_kind}: accepted {mutation} on projection"
                );
            }
        }
    }

    #[test]
    fn no_verdict_timeout_disposition_is_typed_and_contradictions_refuse() {
        let mut timeout = no_verdict_row();
        timeout.series.outcome = SeriesOutcome::Timeout;
        timeout.series.result = Some(ObservedResult::Timeout);
        let disposition = &mut timeout
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        disposition.disposition = SeriesOutcome::Timeout;
        disposition.status = None;
        disposition.signal = Some(15);
        disposition.timed_out = true;
        timeout.validate_for_write().unwrap();

        let mut missing_timeout = timeout.clone();
        missing_timeout
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .timed_out = false;
        assert!(
            missing_timeout
                .validate_for_read()
                .unwrap_err()
                .contains("not_run evidence must carry")
        );

        let mut invented_result = no_verdict_row();
        invented_result.series.result = Some(ObservedResult::CrashError);
        assert!(
            invented_result
                .validate_for_read()
                .unwrap_err()
                .contains("result=Some(CrashError)")
        );

        for status in [Some(0), None] {
            let mut incomplete = no_verdict_row();
            let disposition = &mut incomplete
                .series
                .no_verdict_evidence
                .as_mut()
                .unwrap()
                .attempts[0];
            disposition.status = status;
            disposition.signal = None;
            assert!(incomplete.validate_for_read().unwrap_err().contains(
                "either one nonzero process disposition or an explicit pre-launch timeout"
            ));
        }

        let mut prelaunch_timeout = no_verdict_row();
        let disposition = &mut prelaunch_timeout
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        disposition.disposition = SeriesOutcome::Timeout;
        disposition.status = None;
        disposition.signal = None;
        disposition.timed_out = true;
        prelaunch_timeout.validate_for_write().unwrap();

        let mut wrong_prelaunch_kind = prelaunch_timeout;
        wrong_prelaunch_kind
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .error_kind = Some("infrastructure".into());
        assert!(
            wrong_prelaunch_kind
                .validate_for_write()
                .unwrap_err()
                .contains("explicit pre-launch timeout")
        );

        let mut historical_errored = row(SeriesSchema::V3);
        historical_errored.series.outcome = SeriesOutcome::Errored;
        historical_errored.series.result = Some(ObservedResult::CrashError);
        historical_errored.series.failure_class = Some(FailureClass::ProductFailure);
        historical_errored.validate_for_read().unwrap();
        assert!(
            historical_errored
                .validate_for_write()
                .unwrap_err()
                .contains("must carry no_verdict_evidence")
        );

        historical_errored.series.attempt = Some(1);
        historical_errored.series.no_verdict_evidence = Some(SeriesNoVerdictEvidence {
            evidence_sha256: "e".repeat(64),
            attempts: vec![SeriesAttemptDisposition {
                index: "1".into(),
                kind: SeriesNoVerdictKind::FirstRunRejected,
                detail: None,
                attempt_outcome: "FAIL".into(),
                disposition: SeriesOutcome::NoResult,
                error_kind: None,
                status: Some(125),
                signal: None,
                timed_out: false,
                verification_report_sha256: Some("f".repeat(64)),
            }],
        });
        historical_errored.validate_for_write().unwrap();

        let mut unspecified = no_verdict_row();
        unspecified
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .kind = SeriesNoVerdictKind::Unspecified;
        unspecified.validate_for_write().unwrap();

        let mut refused = no_verdict_row();
        let refusal_detail = "the second log was truncated at the configured size bound";
        let refused_disposition = &mut refused
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        refused_disposition.kind = SeriesNoVerdictKind::ComparisonRefused;
        refused_disposition.detail = Some(refusal_detail.into());
        refused.validate_for_write().unwrap();
        let serialized = serde_json::to_value(&refused).unwrap();
        assert_eq!(
            serialized["series"]["no_verdict_evidence"]["attempts"][0]["detail"],
            refusal_detail
        );

        let mut refused_without_detail = refused.clone();
        refused_without_detail
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .detail = None;
        assert!(
            refused_without_detail
                .validate_for_write()
                .unwrap_err()
                .contains("must carry nonempty detail")
        );

        let mut refused_without_typed_error = refused;
        refused_without_typed_error
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .error_kind = Some("cli-error".into());
        assert!(
            refused_without_typed_error
                .validate_for_write()
                .unwrap_err()
                .contains("comparison_refused evidence")
        );

        let mut noncanonical = no_verdict_row();
        let disposition = &mut noncanonical
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0];
        disposition.kind = SeriesNoVerdictKind::NoncanonicalMatch;
        disposition.attempt_outcome = "PASS".into();
        disposition.error_kind = None;
        disposition.status = Some(0);
        noncanonical.validate_for_write().unwrap();

        let mut noncanonical_nonzero = noncanonical;
        noncanonical_nonzero
            .series
            .no_verdict_evidence
            .as_mut()
            .unwrap()
            .attempts[0]
            .status = Some(1);
        assert!(
            noncanonical_nonzero
                .validate_for_read()
                .unwrap_err()
                .contains("noncanonical_match evidence")
        );
    }

    fn pressure_row() -> SeriesRow {
        let mut value = row(SeriesSchema::V3);
        value.producer = SeriesProducer::PressureTest;
        value.series.attempt = Some(1);
        value.series.pressure_evidence = Some(SeriesPressureEvidence {
            evidence_sha256: "b".repeat(64),
            attempts: vec![SeriesPressureAttempt {
                index: "1".into(),
                outcome: "PASS".into(),
                error_kind: None,
                status: Some(0),
                signal: None,
                timed_out: false,
                comparison: Some(SeriesPressureComparison {
                    verdict: Verdict::Matched,
                    canonical: true,
                    report_sha256: "c".repeat(64),
                    no_result_kind: None,
                }),
            }],
        });
        value
    }

    #[test]
    fn pressure_history_preserves_framework_result_and_declared_subruns() {
        let mut value = pressure_row();
        value.validate_for_write().unwrap();
        let mut second = value.series.pressure_evidence.as_ref().unwrap().attempts[0].clone();
        second.index = "2".into();
        value
            .series
            .pressure_evidence
            .as_mut()
            .unwrap()
            .attempts
            .push(second);
        value.validate_for_write().unwrap();
        // Retain the producer's PASS while exposing its earlier adverse subrun.
        let earlier = &mut value.series.pressure_evidence.as_mut().unwrap().attempts[0];
        earlier.outcome = "FAIL".into();
        earlier.status = Some(1);
        earlier.comparison.as_mut().unwrap().verdict = Verdict::Diverged;
        value.validate_for_write().unwrap();
        assert_eq!(value.series.result, Some(ObservedResult::Pass));
        let raw = serde_json::to_vec(&value).unwrap();
        let decoded: SeriesRow = serde_json::from_slice(&raw).unwrap();
        assert_eq!(decoded, value);
        // Chaos may accept an intentionally nonzero guest status.
        let mut chaos = pressure_row();
        chaos.series.cell = "fixture/test/chaos/ptrace".into();
        chaos.series.pressure_evidence.as_mut().unwrap().attempts[0].status = Some(17);
        chaos.validate_for_write().unwrap();
        let mut custom = pressure_row();
        custom.series.cell = "fixture/test/custom/ptrace".into();
        let inner = &mut custom.series.pressure_evidence.as_mut().unwrap().attempts[0];
        inner.comparison = None;
        inner.status = None;
        inner.signal = Some(11);
        custom.validate_for_write().unwrap();
    }

    #[test]
    fn pressure_history_binds_schema_producer_outer_attempt_and_source() {
        let good = pressure_row();
        for schema in [SeriesSchema::V1, SeriesSchema::V2] {
            let mut bad = good.clone();
            bad.schema = schema;
            assert!(
                bad.validate_for_read()
                    .unwrap_err()
                    .contains("pressure_evidence")
            );
        }
        for producer in [SeriesProducer::Validate, SeriesProducer::HermitRepeat] {
            let mut bad = good.clone();
            bad.producer = producer;
            assert!(
                bad.validate_for_read()
                    .unwrap_err()
                    .contains("pressure_evidence")
            );
        }
        for attempt in [None, Some(0)] {
            let mut bad = good.clone();
            bad.series.attempt = attempt;
            assert!(bad.validate_for_read().is_err());
        }
        let mut collapsed = good.clone();
        collapsed.series.num_runs = 2;
        assert!(
            collapsed
                .validate_for_read()
                .unwrap_err()
                .contains("uncompressed")
        );
        collapsed = good.clone();
        collapsed.series.last_run_index = Some(1);
        assert!(
            collapsed
                .validate_for_read()
                .unwrap_err()
                .contains("uncompressed")
        );
        for digest in [
            "",
            "ABCDEF",
            &"B".repeat(64),
            &"0".repeat(63),
            &"g".repeat(64),
        ] {
            let mut bad = good.clone();
            bad.series
                .pressure_evidence
                .as_mut()
                .unwrap()
                .evidence_sha256 = digest.into();
            assert!(
                bad.validate_for_read()
                    .unwrap_err()
                    .contains("evidence_sha256")
            );
            let mut bad = good.clone();
            bad.series.pressure_evidence.as_mut().unwrap().attempts[0]
                .comparison
                .as_mut()
                .unwrap()
                .report_sha256 = digest.into();
            assert!(
                bad.validate_for_read()
                    .unwrap_err()
                    .contains("report_sha256")
            );
        }
    }

    #[test]
    fn pressure_history_refuses_missing_duplicate_and_contradictory_inner_facts() {
        let good = pressure_row();
        let mutate = |edit: fn(&mut SeriesPressureAttempt)| {
            let mut bad = good.clone();
            edit(&mut bad.series.pressure_evidence.as_mut().unwrap().attempts[0]);
            assert!(
                bad.validate_for_read().is_err(),
                "accepted {:?}",
                bad.series.pressure_evidence
            );
        };
        for edit in [
            (|a: &mut SeriesPressureAttempt| a.index.clear()) as fn(&mut SeriesPressureAttempt),
            |a| a.outcome = "UNKNOWN".into(),
            |a| a.status = Some(-1),
            |a| a.signal = Some(0),
            |a| a.signal = Some(9),
            |a| a.error_kind = Some("".into()),
            |a| a.error_kind = Some("infrastructure".into()),
            |a| a.timed_out = true,
            |a| a.comparison = None,
            |a| a.status = None,
            |a| a.status = Some(7),
            |a| a.comparison.as_mut().unwrap().no_result_kind = Some(SeriesNoVerdictKind::NotRun),
            |a| a.comparison.as_mut().unwrap().verdict = Verdict::NoResult,
            |a| a.comparison.as_mut().unwrap().verdict = Verdict::InfrastructureError,
        ] {
            mutate(edit);
        }
        let mut bad = good.clone();
        bad.series
            .pressure_evidence
            .as_mut()
            .unwrap()
            .attempts
            .clear();
        assert!(bad.validate_for_read().is_err());
        let mut bad = good.clone();
        let inner = bad.series.pressure_evidence.as_ref().unwrap().attempts[0].clone();
        bad.series
            .pressure_evidence
            .as_mut()
            .unwrap()
            .attempts
            .push(inner);
        assert!(bad.validate_for_read().unwrap_err().contains("unique"));
        for (field, unknown) in [("verdict", "invented"), ("no_result_kind", "invented")] {
            let mut raw = serde_json::to_value(&good).unwrap();
            raw["series"]["pressure_evidence"]["attempts"][0]["comparison"][field] =
                serde_json::json!(unknown);
            assert!(serde_json::from_value::<SeriesRow>(raw).is_err());
        }
        for outcome in ["PASS", "FAIL", "ERROR"] {
            let mut native = good.clone();
            native.series.cell = "fixture/test/naked/native".into();
            let inner = &mut native.series.pressure_evidence.as_mut().unwrap().attempts[0];
            inner.outcome = outcome.into();
            inner.comparison = None;
            inner.status = None;
            assert!(
                native.validate_for_read().is_err(),
                "missing {outcome} process"
            );
        }
        let mut native = good.clone();
        native.series.cell = "fixture/test/naked/native".into();
        let inner = &mut native.series.pressure_evidence.as_mut().unwrap().attempts[0];
        inner.outcome = "ERROR".into();
        inner.comparison = None;
        inner.status = None;
        inner.timed_out = true;
        inner.error_kind = Some("cpu-timeout".into());
        native.validate_for_read().unwrap();
        let mut raw = serde_json::to_value(&good).unwrap();
        raw["series"]["pressure_evidence"]["attempts"][0]["timed_out"] = serde_json::json!("false");
        assert!(serde_json::from_value::<SeriesRow>(raw).is_err());
    }

    #[test]
    fn pressure_history_cross_checks_no_verdict_facts_without_replacing_them() {
        let mut value = no_verdict_row();
        value.producer = SeriesProducer::PressureTest;
        value.series.pressure_evidence = Some(SeriesPressureEvidence {
            evidence_sha256: "b".repeat(64),
            attempts: vec![SeriesPressureAttempt {
                index: "1".into(),
                outcome: "ERROR".into(),
                error_kind: Some("incomplete-verification-evidence".into()),
                status: Some(125),
                signal: None,
                timed_out: false,
                comparison: Some(SeriesPressureComparison {
                    verdict: Verdict::NoResult,
                    canonical: false,
                    report_sha256: "c".repeat(64),
                    no_result_kind: Some(SeriesNoVerdictKind::NotRun),
                }),
            }],
        });
        value.validate_for_write().unwrap();
        let mut mismatch = value.clone();
        mismatch
            .series
            .pressure_evidence
            .as_mut()
            .unwrap()
            .evidence_sha256 = "d".repeat(64);
        assert!(
            mismatch
                .validate_for_read()
                .unwrap_err()
                .contains("different CellResults")
        );
        for edit in [
            (|a: &mut SeriesPressureAttempt| a.index = "2".into())
                as fn(&mut SeriesPressureAttempt),
            |a| a.status = Some(126),
            |a| a.error_kind = Some("cpu-timeout".into()),
            |a| a.comparison.as_mut().unwrap().report_sha256 = "d".repeat(64),
        ] {
            let mut bad = value.clone();
            edit(&mut bad.series.pressure_evidence.as_mut().unwrap().attempts[0]);
            assert!(bad.validate_for_read().is_err());
        }
        // Current prelaunch CPU and wall timeouts retain the lack of a process.
        for kind in ["cpu-timeout", "wall-timeout"] {
            let mut timed = value.clone();
            timed.series.outcome = SeriesOutcome::Timeout;
            timed.series.result = Some(ObservedResult::Timeout);
            let inner = &mut timed.series.pressure_evidence.as_mut().unwrap().attempts[0];
            inner.timed_out = true;
            inner.status = None;
            inner.error_kind = Some(kind.into());
            let inner = &mut timed.series.no_verdict_evidence.as_mut().unwrap().attempts[0];
            inner.timed_out = true;
            inner.status = None;
            inner.error_kind = Some(kind.into());
            inner.disposition = SeriesOutcome::Timeout;
            timed.validate_for_write().unwrap();
        }
    }

    #[test]
    fn pressure_history_retains_unspecified_and_refused_comparison_contracts() {
        for kind in [
            SeriesNoVerdictKind::Unspecified,
            SeriesNoVerdictKind::ComparisonRefused,
        ] {
            for mode in ["verify", "replay", "chaos"] {
                let mut value = no_verdict_row();
                value.producer = SeriesProducer::PressureTest;
                value.series.cell = format!("fixture/test/{mode}/ptrace");
                let evidence = value.series.no_verdict_evidence.as_mut().unwrap();
                let disposition = &mut evidence.attempts[0];
                disposition.kind = kind;
                disposition.detail = (kind == SeriesNoVerdictKind::ComparisonRefused)
                    .then(|| "the second log was truncated at its size bound".into());
                value.series.pressure_evidence = Some(SeriesPressureEvidence {
                    evidence_sha256: evidence.evidence_sha256.clone(),
                    attempts: vec![SeriesPressureAttempt {
                        index: disposition.index.clone(),
                        outcome: disposition.attempt_outcome.clone(),
                        error_kind: disposition.error_kind.clone(),
                        status: disposition.status,
                        signal: disposition.signal,
                        timed_out: disposition.timed_out,
                        comparison: Some(SeriesPressureComparison {
                            verdict: Verdict::NoResult,
                            canonical: false,
                            report_sha256: disposition.verification_report_sha256.clone().unwrap(),
                            no_result_kind: Some(kind),
                        }),
                    }],
                });
                value.validate_for_write().unwrap();
                let serialized = serde_json::to_value(&value).unwrap();
                let decoded: SeriesRow = serde_json::from_value(serialized.clone()).unwrap();
                decoded.validate_for_read().unwrap();
                assert_eq!(serde_json::to_value(decoded).unwrap(), serialized);

                let mut signaled = value.clone();
                let disposition = &mut signaled
                    .series
                    .no_verdict_evidence
                    .as_mut()
                    .unwrap()
                    .attempts[0];
                disposition.status = None;
                disposition.signal = Some(11);
                let inner = &mut signaled.series.pressure_evidence.as_mut().unwrap().attempts[0];
                inner.status = None;
                inner.signal = Some(11);
                signaled.validate_for_write().unwrap();

                for edit in [
                    (|a: &mut SeriesPressureAttempt| a.outcome = "PASS".into())
                        as fn(&mut SeriesPressureAttempt),
                    |a| a.outcome = "FAIL".into(),
                    |a| a.error_kind = None,
                    |a| a.error_kind = Some(" ".into()),
                    |a| a.status = Some(0),
                    |a| a.status = None,
                    |a| a.signal = Some(11),
                    |a| a.timed_out = true,
                    |a| a.comparison.as_mut().unwrap().canonical = true,
                    |a| a.comparison.as_mut().unwrap().no_result_kind = None,
                    |a| a.comparison.as_mut().unwrap().report_sha256 = "d".repeat(64),
                ] {
                    let mut bad = value.clone();
                    edit(&mut bad.series.pressure_evidence.as_mut().unwrap().attempts[0]);
                    assert!(bad.validate_for_read().is_err(), "{kind:?} {mode}");
                }
                let mut wrong_kind = value.clone();
                wrong_kind
                    .series
                    .pressure_evidence
                    .as_mut()
                    .unwrap()
                    .attempts[0]
                    .comparison
                    .as_mut()
                    .unwrap()
                    .no_result_kind = Some(if kind == SeriesNoVerdictKind::Unspecified {
                    SeriesNoVerdictKind::ComparisonRefused
                } else {
                    SeriesNoVerdictKind::Unspecified
                });
                assert!(
                    wrong_kind
                        .validate_for_read()
                        .unwrap_err()
                        .contains("contradicts the same no_verdict_evidence")
                );

                let mut different_error = value;
                different_error
                    .series
                    .no_verdict_evidence
                    .as_mut()
                    .unwrap()
                    .attempts[0]
                    .error_kind = Some("cli-error".into());
                different_error
                    .series
                    .pressure_evidence
                    .as_mut()
                    .unwrap()
                    .attempts[0]
                    .error_kind = Some("cli-error".into());
                if kind == SeriesNoVerdictKind::Unspecified {
                    different_error.validate_for_write().unwrap();
                } else {
                    assert!(different_error.validate_for_read().is_err());
                }
            }
        }
    }

    #[test]
    fn historical_pressure_rows_without_inner_history_remain_readable() {
        for schema in [SeriesSchema::V1, SeriesSchema::V2, SeriesSchema::V3] {
            let mut historical = row(schema);
            historical.producer = SeriesProducer::PressureTest;
            historical.series.attempt = Some(1);
            historical.validate_for_read().unwrap();
            let raw = serde_json::to_value(&historical).unwrap();
            assert!(raw["series"].get("pressure_evidence").is_none());
            let decoded: SeriesRow = serde_json::from_value(raw).unwrap();
            assert!(decoded.series.pressure_evidence.is_none());
        }
    }

    #[test]
    fn v3_accepts_only_matching_typed_infrastructure_classifications() {
        for result in [
            ObservedResult::SandboxDenied,
            ObservedResult::InfrastructureError,
        ] {
            let mut fixture = no_verdict_row();
            fixture.series.outcome = SeriesOutcome::Errored;
            fixture.series.result = Some(result);
            fixture.series.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
            fixture.validate_for_read().unwrap();
            fixture.validate_for_write().unwrap();
            fixture.validate_for_projection().unwrap();

            let encoded = serde_json::to_string(&fixture).unwrap();
            let decoded: SeriesRow = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded.series.result, Some(result));
            decoded.validate_for_read().unwrap();

            fixture.series.failure_class = Some(FailureClass::ProductFailure);
            let error = fixture.validate_for_write().unwrap_err();
            assert!(error.contains(&format!("{result:?}")), "{error}");
            assert!(error.contains("ProductFailure"), "{error}");
        }
    }
}
