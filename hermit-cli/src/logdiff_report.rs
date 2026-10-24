//! Typed machine-readable reports produced by `hermit log-diff --json`.
//!
//! This lives in the library rather than the CLI module because the manifest
//! runner and scorecard consume the report.  Keeping one producer-owned type is
//! what prevents a textual `log-diff` success banner from becoming a parity
//! verdict after the producer changes shape.

use serde::Deserialize;
use serde::Serialize;

/// Schema written by the current producer.
///
/// Schema 2 added [`LogDiffReport::matched_prefix_records`]. Schema-1 reports
/// remain readable (historical schema-10 ledger rows embed them); they simply
/// carry no matched prefix.
pub const LOG_DIFF_REPORT_SCHEMA: u64 = 2;

/// Every schema a reader accepts, oldest first.
pub const READABLE_LOG_DIFF_REPORT_SCHEMAS: [u64; 2] = [1, LOG_DIFF_REPORT_SCHEMA];

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
    /// (`evidence_emit_image_initialization`, reverie-dbt native/client.c).
    /// Offline `hermit log-diff` applies this selection to archived evidence
    /// logs when asked. Live DBT verification does not select it: it compares
    /// under `AllRecordsV1`, with initialization records at their stream
    /// positions, and also compares Reverie's authenticated initialization
    /// count as separate typed evidence.
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
    /// Number of leading selected messages that are equal on both sides, in
    /// the units of [`Self::selected_messages`] (schema 2 and later).
    ///
    /// On a match it equals both selected counts. After a divergence it is the
    /// count of selected messages before the first difference; when one
    /// selected stream is a strict prefix of the other it is the shorter
    /// length, because the extra messages are the divergence. It is `None`
    /// when nothing was measured: a refusal, a pending report, an empty
    /// comparison, or a schema-1 report.
    ///
    /// This is deliberately not `first_divergent_record - 1`:
    /// `first_divergent_record` is a raw log-record index that also counts
    /// records the comparison did not select, and `records.compared` is
    /// `min(available_left, available_right)`, a bound on what was read rather
    /// than a matched length.
    ///
    /// Omitted rather than written as `null` when absent, so that a schema-1
    /// report re-serializes to exactly the bytes it was read from. Stored
    /// ledger rows hash the re-serialized typed witness into their evidence
    /// identity; writing a new `null` key would change every historical hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_prefix_records: Option<usize>,
    pub first_divergent_syscall: Option<u64>,
    pub first_divergent_scheduler_turn: Option<u64>,
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    pub first_divergent_left_message: Option<String>,
    pub first_divergent_right_message: Option<String>,
}

impl LogDiffReport {
    /// Refuse a schema this reader does not know. Every reader of a stored
    /// report goes through this before trusting its fields.
    pub fn require_readable_schema(&self) -> Result<(), String> {
        if READABLE_LOG_DIFF_REPORT_SCHEMAS.contains(&self.schema) {
            Ok(())
        } else {
            Err(format!(
                "log-diff report schema must be one of {READABLE_LOG_DIFF_REPORT_SCHEMAS:?}, got {}",
                self.schema
            ))
        }
    }

    /// Check [`Self::matched_prefix_records`] against the verdict and counts.
    ///
    /// Schema 1 predates the field, so it must be absent there. From schema 2 a
    /// comparison verdict must carry it, a match must have matched every
    /// selected message on both sides, and a divergence must have matched
    /// fewer than the longer side selected. A comparison that selected nothing
    /// on either side measured nothing, so it carries no prefix whatever its
    /// verdict: `0 of 0` is not a full match.
    ///
    /// The schema is checked here too, so that a caller using this check on its
    /// own cannot have an unknown schema read as schema 2.
    pub fn require_consistent_matched_prefix(&self) -> Result<(), String> {
        self.require_readable_schema()?;
        let selected = &self.selected_messages;
        let longer = selected.left.max(selected.right);
        let shorter = selected.left.min(selected.right);
        match (self.schema, self.matched_prefix_records) {
            (1, None) => Ok(()),
            (1, Some(_)) => Err("schema-1 log-diff report carries a matched prefix".into()),
            (_, None)
                if matches!(
                    self.verdict,
                    LogDiffVerdict::Matched
                        | LogDiffVerdict::Diverged
                        | LogDiffVerdict::IdenticalSoFar
                ) && longer > 0 =>
            {
                Err(format!(
                    "log-diff {:?} report omitted its matched prefix",
                    self.verdict
                ))
            }
            (_, None) => Ok(()),
            (_, Some(prefix)) if prefix > shorter => Err(format!(
                "log-diff matched prefix {prefix} exceeds the shorter selected stream ({shorter})"
            )),
            (_, Some(prefix)) => match self.verdict {
                LogDiffVerdict::NoResult
                | LogDiffVerdict::Refused
                | LogDiffVerdict::NoComparableMessages => Err(format!(
                    "log-diff {:?} report carries a matched prefix",
                    self.verdict
                )),
                _ if longer == 0 => Err(format!(
                    "log-diff {:?} report carries a matched prefix over 0 | 0 selected messages",
                    self.verdict
                )),
                LogDiffVerdict::Matched | LogDiffVerdict::IdenticalSoFar
                    if prefix != selected.left || prefix != selected.right =>
                {
                    Err(format!(
                        "log-diff {:?} report matched {prefix} of {} | {} selected messages",
                        self.verdict, selected.left, selected.right
                    ))
                }
                LogDiffVerdict::Diverged if prefix >= longer => Err(format!(
                    "log-diff divergence matched all {longer} selected messages"
                )),
                _ => Ok(()),
            },
        }
    }

    /// Require the exact non-lossy policy used for a cross-backend parity
    /// verdict.  A report may be typed yet still be unsuitable (empty,
    /// truncated, relaxed, or produced under another record envelope).
    pub fn require_cross_backend_evidence(&self) -> Result<(), String> {
        self.require_readable_schema()?;
        self.require_consistent_matched_prefix()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A cross-backend match exactly as the schema-1 producer wrote it: no
    /// `matched_prefix_records` key, and `first_divergent_record` omitted.
    const SCHEMA_1_MATCH: &str = r#"{"schema":1,"verdict":"matched","selected_messages":{"left":3,"right":3},"records":{"compared":5,"available_left":5,"available_right":6,"withheld_incomplete_tail":false},"inputs":{"left":{"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bytes":210},"right":{"sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","bytes":252}},"comparison":{"stream":"info","record_envelope":"cross_backend_detcore_v1","unsafe_strip_lines":false,"canonicalize_host_addresses":true,"require_structured_events":true,"ignored_line_substrings":[],"skip_commit":false,"skip_detlog":false,"included_detlog_kinds":["syscall","syscall_result","other"],"git_diff":false},"first_divergent_syscall":null,"first_divergent_scheduler_turn":null,"first_divergent_virtual_nanoseconds":null,"first_divergent_left_message":null,"first_divergent_right_message":null}"#;

    fn schema_1_match() -> LogDiffReport {
        serde_json::from_str(SCHEMA_1_MATCH).expect("a schema-1 report must still parse")
    }

    fn schema_2(
        verdict: LogDiffVerdict,
        left: usize,
        right: usize,
        prefix: Option<usize>,
    ) -> LogDiffReport {
        LogDiffReport {
            schema: LOG_DIFF_REPORT_SCHEMA,
            verdict,
            selected_messages: LogDiffMessageCounts { left, right },
            matched_prefix_records: prefix,
            first_divergent_record: (verdict == LogDiffVerdict::Diverged)
                .then(|| prefix.unwrap_or(0) + 1),
            ..schema_1_match()
        }
    }

    #[test]
    fn a_schema_1_report_still_parses_and_carries_no_matched_prefix() {
        let report = schema_1_match();
        assert_eq!(report.schema, 1);
        assert_eq!(report.matched_prefix_records, None);
        // Byte-identical re-serialization: ledger evidence identities hash the
        // re-serialized typed witness, so a schema-1 row must keep its bytes.
        assert_eq!(serde_json::to_string(&report).unwrap(), SCHEMA_1_MATCH);
        report.require_readable_schema().unwrap();
        report.require_consistent_matched_prefix().unwrap();
        report.require_cross_backend_evidence().unwrap();

        // A schema-1 report claiming the schema-2 field is not a report any
        // producer wrote.
        let mut forged = report;
        forged.matched_prefix_records = Some(3);
        assert!(forged.require_cross_backend_evidence().is_err());
    }

    #[test]
    fn schema_2_round_trips_and_an_unknown_schema_or_field_is_refused() {
        let report = schema_2(LogDiffVerdict::Matched, 3, 3, Some(3));
        let text = serde_json::to_string(&report).unwrap();
        assert!(text.contains(r#""schema":2"#));
        assert!(text.contains(r#""matched_prefix_records":3"#));
        let parsed: LogDiffReport = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, report);
        parsed.require_cross_backend_evidence().unwrap();

        let future = LogDiffReport {
            schema: 3,
            ..report
        };
        assert!(future.require_readable_schema().is_err());
        assert!(future.require_cross_backend_evidence().is_err());

        let extra = SCHEMA_1_MATCH.replacen('{', r#"{"matched_prefix":1,"#, 1);
        assert!(serde_json::from_str::<LogDiffReport>(&extra).is_err());
    }

    #[test]
    fn a_schema_2_matched_prefix_must_agree_with_the_verdict() {
        // Identical: the whole of both streams.
        schema_2(LogDiffVerdict::Matched, 4, 4, Some(4))
            .require_consistent_matched_prefix()
            .unwrap();
        // Divergence at the first selected message, and further in.
        schema_2(LogDiffVerdict::Diverged, 4, 4, Some(0))
            .require_consistent_matched_prefix()
            .unwrap();
        schema_2(LogDiffVerdict::Diverged, 4, 4, Some(2))
            .require_consistent_matched_prefix()
            .unwrap();
        // A strict prefix: diverged, matched over the shorter side only.
        schema_2(LogDiffVerdict::Diverged, 2, 5, Some(2))
            .require_consistent_matched_prefix()
            .unwrap();

        // Follow mode stopped before the streams diverged: every selected
        // message on both sides matched.
        schema_2(LogDiffVerdict::IdenticalSoFar, 4, 4, Some(4))
            .require_consistent_matched_prefix()
            .unwrap();

        for (label, report, message) in [
            (
                "a match missing its prefix",
                schema_2(LogDiffVerdict::Matched, 4, 4, None),
                "log-diff Matched report omitted its matched prefix",
            ),
            (
                "a partial match",
                schema_2(LogDiffVerdict::Matched, 4, 4, Some(3)),
                "log-diff Matched report matched 3 of 4 | 4 selected messages",
            ),
            (
                "a divergence matching everything",
                schema_2(LogDiffVerdict::Diverged, 4, 4, Some(4)),
                "log-diff divergence matched all 4 selected messages",
            ),
            (
                "a prefix past the shorter side",
                schema_2(LogDiffVerdict::Diverged, 2, 5, Some(3)),
                "log-diff matched prefix 3 exceeds the shorter selected stream (2)",
            ),
            (
                "a divergence missing its prefix",
                schema_2(LogDiffVerdict::Diverged, 4, 4, None),
                "log-diff Diverged report omitted its matched prefix",
            ),
            (
                "a refusal with a prefix",
                schema_2(LogDiffVerdict::Refused, 0, 0, Some(0)),
                "log-diff Refused report carries a matched prefix",
            ),
            (
                "a no-result report with a prefix",
                schema_2(LogDiffVerdict::NoResult, 0, 0, Some(0)),
                "log-diff NoResult report carries a matched prefix",
            ),
            (
                "an empty comparison with a prefix",
                schema_2(LogDiffVerdict::NoComparableMessages, 0, 0, Some(0)),
                "log-diff NoComparableMessages report carries a matched prefix",
            ),
            (
                "a follow-mode report missing its prefix",
                schema_2(LogDiffVerdict::IdenticalSoFar, 4, 4, None),
                "log-diff IdenticalSoFar report omitted its matched prefix",
            ),
            (
                "a partial follow-mode match",
                schema_2(LogDiffVerdict::IdenticalSoFar, 4, 4, Some(3)),
                "log-diff IdenticalSoFar report matched 3 of 4 | 4 selected messages",
            ),
            // 0 == 0 == 0 satisfies "equal to both counts", so these two passed
            // before the empty-selection rule, as a full match of nothing.
            (
                "a match of nothing",
                schema_2(LogDiffVerdict::Matched, 0, 0, Some(0)),
                "log-diff Matched report carries a matched prefix over 0 | 0 selected messages",
            ),
            (
                "a follow-mode match of nothing",
                schema_2(LogDiffVerdict::IdenticalSoFar, 0, 0, Some(0)),
                "log-diff IdenticalSoFar report carries a matched prefix over 0 | 0 selected messages",
            ),
            (
                "a divergence of nothing",
                schema_2(LogDiffVerdict::Diverged, 0, 0, Some(0)),
                "log-diff Diverged report carries a matched prefix over 0 | 0 selected messages",
            ),
            // The check on its own must not read an unknown schema as schema 2.
            (
                "a future schema",
                LogDiffReport {
                    schema: 3,
                    ..schema_2(LogDiffVerdict::Matched, 4, 4, Some(4))
                },
                "log-diff report schema must be one of [1, 2], got 3",
            ),
            (
                "schema zero",
                LogDiffReport {
                    schema: 0,
                    ..schema_2(LogDiffVerdict::Matched, 4, 4, Some(4))
                },
                "log-diff report schema must be one of [1, 2], got 0",
            ),
        ] {
            assert_eq!(
                report.require_consistent_matched_prefix(),
                Err(message.to_string()),
                "{label} must be refused for its own reason"
            );
        }
        // Nothing compared: nothing measured, and that is consistent. The
        // follow-mode timeout can stop before either side selected anything.
        for verdict in [
            LogDiffVerdict::NoComparableMessages,
            LogDiffVerdict::IdenticalSoFar,
        ] {
            schema_2(verdict, 0, 0, None)
                .require_consistent_matched_prefix()
                .unwrap();
        }
    }
}
