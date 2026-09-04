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
    AllRecordsV1,
    DbtEvidenceTransportV1,
    /// Shared Detcore observations which exist on every real backend.  The
    /// predicate excludes backend launch/transport/lifecycle records, but it
    /// does not filter or normalize any Detcore payload: virtual time, RCBs,
    /// syscall values, flags, sizes, and I/O-buffer hashes remain exact.
    CrossBackendDetcoreV1,
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
        if self.selected_messages.left == 0
            || self.selected_messages.right == 0
            || self.records.compared == 0
        {
            return Err("log-diff parity evidence compared no shared Detcore INFO records".into());
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
