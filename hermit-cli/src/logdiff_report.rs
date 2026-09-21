//! Typed machine-readable reports produced by `hermit log-diff --json`.
//!
//! This lives in the library rather than the CLI module because the manifest
//! runner and scorecard consume the report.  Keeping one producer-owned type is
//! what prevents a textual `log-diff` success banner from becoming a parity
//! verdict after the producer changes shape.

use serde::Deserialize;
use serde::Serialize;

pub const LOG_DIFF_REPORT_SCHEMA: u64 = 1;

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "snake_case")]
pub enum RecordEnvelopePolicy {
    /// Preserve every parsed log record.
    AllRecordsV1,
    /// Exclude only records emitted by the DBT evidence transport about
    /// itself. Those records are real and present in a live evidence stream
    /// (`evidence_emit_image_initialization`, reverie-dbt native/client.c),
    /// but their host-arrival order is not guest behavior. Live DBT verification
    /// uses Reverie's authenticated initialization count as separate typed
    /// evidence and selects this policy for the remaining records. Offline
    /// `hermit log-diff` applies the same selection to archived evidence logs.
    DbtEvidenceTransportV1,
    /// Select only records whose target is Detcore or one of its modules;
    /// comparison then selects INFO. Every other target is excluded, including
    /// shared records emitted outside Detcore. Detcore payloads remain exact:
    /// virtual time, RCBs, syscall values, flags, sizes, and I/O-buffer hashes.
    CrossBackendDetcoreV1,
    /// A predicate whose semantics are not one of the named canonical policies.
    CallerDefined,
}

impl RecordEnvelopePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllRecordsV1 => "all_records_v1",
            Self::DbtEvidenceTransportV1 => "dbt_evidence_transport_v1",
            Self::CrossBackendDetcoreV1 => "cross_backend_detcore_v1",
            Self::CallerDefined => "caller_defined",
        }
    }

    /// Whether this envelope may support same-backend bitwise parity.
    pub fn is_canonical(self) -> bool {
        matches!(self, Self::AllRecordsV1 | Self::DbtEvidenceTransportV1)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
#[serde(rename_all = "snake_case")]
pub enum LogDiffVerdict {
    NoResult,
    Refused,
    Matched,
    IdenticalSoFar,
    Diverged,
    NoComparableMessages,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogDiffMessageCounts {
    pub left: usize,
    pub right: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogDiffRecords {
    pub compared: usize,
    pub available_left: usize,
    pub available_right: usize,
    pub withheld_incomplete_tail: bool,
}

/// Identity of the exact bytes captured and decoded by the comparator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogDiffInput {
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogDiffInputs {
    pub left: LogDiffInput,
    pub right: LogDiffInput,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogDiffComparison {
    pub stream: String,
    pub record_envelope: RecordEnvelopePolicy,
    pub unsafe_strip_lines: bool,
    pub canonicalize_host_addresses: bool,
    pub require_structured_events: bool,
    pub ignored_line_substrings: Vec<String>,
    pub skip_commit: bool,
    pub skip_detlog: bool,
    pub included_detlog_kinds: Vec<String>,
    pub git_diff: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogDiffReport {
    pub schema: u64,
    pub verdict: LogDiffVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    pub selected_messages: LogDiffMessageCounts,
    pub records: LogDiffRecords,
    /// Older reports did not identify their captured bytes. They remain
    /// readable, but cannot establish current cross-backend parity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<LogDiffInputs>,
    pub comparison: LogDiffComparison,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_stopped_because: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_divergent_record: Option<usize>,
    pub first_divergent_syscall: Option<u64>,
    pub first_divergent_scheduler_turn: Option<u64>,
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    pub first_divergent_left_message: Option<String>,
    pub first_divergent_right_message: Option<String>,
}

impl LogDiffReport {
    /// Require the exact non-lossy policy used for a cross-backend parity
    /// verdict.  A report may be typed yet still be unsuitable (empty,
    /// truncated, relaxed, or produced under another record envelope).
    pub fn require_cross_backend_evidence(&self) -> Result<(), String> {
        if self.schema != LOG_DIFF_REPORT_SCHEMA {
            return Err(format!(
                "log-diff report schema must be {LOG_DIFF_REPORT_SCHEMA}, got {}",
                self.schema
            ));
        }
        if !matches!(
            self.verdict,
            LogDiffVerdict::Matched | LogDiffVerdict::Diverged
        ) {
            return Err(format!(
                "log-diff did not reach a parity verdict: {:?}",
                self.verdict
            ));
        }
        if self.refusal.is_some()
            || self.follow_stopped_because.is_some()
            || self.records.withheld_incomplete_tail
        {
            return Err("log-diff parity evidence is refused, followed, or incomplete".into());
        }
        let inputs = self.inputs.as_ref().ok_or_else(|| {
            "log-diff parity evidence omitted captured input identities".to_string()
        })?;
        for (label, input) in [("left", &inputs.left), ("right", &inputs.right)] {
            if input.bytes == 0
                || input.sha256.len() != 64
                || !input
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(format!(
                    "log-diff {label} captured input identity is invalid"
                ));
            }
        }
        if self.selected_messages.left == 0
            || self.selected_messages.right == 0
            || self.records.compared == 0
        {
            return Err("log-diff parity evidence compared no shared Detcore INFO records".into());
        }
        // The one-shot producer reads both complete inputs before selecting
        // their shared INFO envelope. Raw record populations can differ across
        // backends; the selected streams of a match cannot.
        if self.records.compared
            != self
                .records
                .available_left
                .min(self.records.available_right)
            || self.selected_messages.left > self.records.available_left
            || self.selected_messages.right > self.records.available_right
        {
            return Err("log-diff parity record counts are incomplete or inconsistent".into());
        }
        if self.verdict == LogDiffVerdict::Matched
            && self.selected_messages.left != self.selected_messages.right
        {
            return Err("log-diff match has unequal selected INFO counts".into());
        }
        let comparison = &self.comparison;
        if comparison.stream != "info"
            || comparison.record_envelope != RecordEnvelopePolicy::CrossBackendDetcoreV1
            || comparison.unsafe_strip_lines
            || !comparison.canonicalize_host_addresses
            || !comparison.require_structured_events
            || !comparison.ignored_line_substrings.is_empty()
            || comparison.skip_commit
            || comparison.skip_detlog
            || comparison.included_detlog_kinds != ["syscall", "syscall_result", "other"]
            || comparison.git_diff
        {
            return Err(
                "log-diff parity evidence did not use CrossBackendDetcoreV1 canonical INFO policy"
                    .into(),
            );
        }
        Ok(())
    }
}
