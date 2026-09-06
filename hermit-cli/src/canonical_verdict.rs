//! Machine-readable verification reports shared by the Hermit producer and its consumers.

use serde::Deserialize;
use serde::Serialize;

/// The verification verdict, independent of the guest's exit status.
///
/// A guest that deterministically exits nonzero can still match, while a guest
/// that exits zero can still diverge. Consumers must read this value rather than
/// infer the verdict from either the process status or human-readable stderr.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The two runs matched on every compared dimension.
    Matched,
    /// The two runs diverged; verification failed.
    Diverged,
    /// Verification stopped before it could reach a verdict.
    NoResult,
    /// Verification completed enough work to name an understood machine or
    /// harness failure, so the receipt is neither a product verdict nor an
    /// unknown no-result.
    InfrastructureError,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Matched => "matched",
            Self::Diverged => "diverged",
            Self::NoResult => "no_result",
            Self::InfrastructureError => "infrastructure_error",
        })
    }
}

/// Why verification did not reach a verdict.
///
/// This distinction is functional evidence. `FirstRunRejected` means the guest
/// ran and its disposition was rejected by `--verify-allow`; `NotRun` is the
/// pre-run stamp left when the invocation died before recording a more specific
/// result. Collapsing both into `no_result` made a completed guest refusal and a
/// container or launcher failure indistinguishable to the result reader.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NoResultReason {
    /// The invocation did not replace the pre-run stamp with a more specific
    /// outcome.
    NotRun,
    /// Run 1 completed and its exit status was rejected by `--verify-allow`.
    FirstRunRejected {
        exit_code: Option<i32>,
        signal: Option<i32>,
        stdout_bytes: u64,
        stderr_bytes: u64,
    },
    /// The log comparator refused to claim either agreement or divergence.
    ComparisonRefused { detail: String },
}

/// The understood infrastructure failure recorded by verification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum InfrastructureError {
    /// A ptrace PMU timer interrupt arrived after its target. Both runs and
    /// their comparison may still be retained, but the result is not admitted.
    SkidOvershoot { count: u64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeStats {
    pub scheduler_turns: u64,
    pub virtual_nanoseconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub syscalls: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationRuntime {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run1: Option<RuntimeStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run2: Option<RuntimeStats>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DbtCountedBranchComparison {
    pub left: u64,
    pub right: u64,
}

impl DbtCountedBranchComparison {
    #[allow(dead_code)] // path-included consumers do not all construct DBT outcomes
    pub fn matched(self) -> bool {
        self.left == self.right
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogCompareStrictness {
    Stripped,
    Canonical,
}

impl std::fmt::Display for LogCompareStrictness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Stripped => "stripped",
            Self::Canonical => "canonical",
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparedLogScope {
    Deterministic,
    Info,
    FullTrace,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordEnvelopeReport {
    AllRecordsV1,
    DbtEvidenceTransportV1,
    CallerDefined,
}

impl RecordEnvelopeReport {
    pub fn is_canonical(self) -> bool {
        matches!(self, Self::AllRecordsV1 | Self::DbtEvidenceTransportV1)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllRecordsV1 => "all_records_v1",
            Self::DbtEvidenceTransportV1 => "dbt_evidence_transport_v1",
            Self::CallerDefined => "caller_defined",
        }
    }
}

/// The comparison fields serialized beside a verification verdict.
///
/// Fields added after the original report are optional here so retained reports
/// remain readable. A current producer writes every one of them, and
/// [`VerificationReport::from_current_json_value`] checks their presence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComparisonReport {
    pub strictness: LogCompareStrictness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub compare_logs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare_io_buffers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_scope: Option<ComparedLogScope>,
    pub record_envelope: RecordEnvelopeReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtualize_time: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_lines: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonicalize_addresses: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_trace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_remainder: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripped_prefixes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonicalizations: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_lines: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_commit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_detlog: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComparedLogMessages {
    pub left: u64,
    pub right: u64,
}

/// The complete machine-readable report written by `--verify-json`.
///
/// This type is shared by the Hermit producer, the manifest runner, the
/// compatibility pressure test, and the scorecard. The serialized field order
/// is retained because existing report bytes are hashed as evidence.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VerificationReport {
    pub verified: bool,
    pub bitwise_parity: bool,
    pub verdict: Verdict,
    #[serde(default)]
    pub no_result_reason: Option<NoResultReason>,
    #[serde(default)]
    pub infrastructure_error: Option<InfrastructureError>,
    /// `None` when the producer reached no verdict. This is a DOCUMENTED
    /// producer state, not a malformed report: `VerificationReport::no_result()`
    /// in hermit-cli sets it, and its doc says "`null` when no verdict was
    /// reached". Declaring it non-optional made a legitimate no-result
    /// deserialise as "unreadable report", which reported an infrastructure
    /// fault where the truth was that nothing had been recorded.
    #[serde(deserialize_with = "present_but_nullable_comparison")]
    pub comparison: Option<ComparisonReport>,
    pub compared_log_messages: Option<ComparedLogMessages>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dbt_counted_branches: Option<DbtCountedBranchComparison>,
    /// Runtime totals for the two compared executions when the producer could
    /// read both run summaries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<VerificationRuntime>,
    /// The guest's exit code when an exited status was available to the
    /// producer. `None` means either that `guest_signal` records a signal
    /// disposition, or that execution did not return a guest status at all;
    /// current producers must not leave this null for `ExitStatus::Exited`.
    #[serde(default)]
    pub guest_exit_code: Option<i32>,
    /// The guest's signal when a signaled status was available to the producer.
    /// When both disposition fields are null, the writer had no guest status.
    #[serde(default)]
    pub guest_signal: Option<i32>,
    /// WHERE the divergence began, in scheduler-turn units. `None` when the
    /// logs matched, when no comparison ran, or when the report predates the
    /// field.
    ///
    /// `#[serde(default)]` is deliberate here and is the OPPOSITE choice from
    /// `ComparisonReport::record_envelope` above. That field is an admission
    /// gate, so a missing value must never be supplied on the producer's
    /// behalf. This one is a diagnostic position: absent means "not located",
    /// which is exactly what `None` says. Requiring it would turn every
    /// retained pre-field report into an unreadable-report ERROR and
    /// misclassify old evidence as an infrastructure fault -- the same trap
    /// documented on `comparison` just above.
    #[serde(default)]
    pub first_divergent_scheduler_turn: Option<u64>,
    /// WHERE the divergence began, in virtual-nanosecond units. Same
    /// tolerant-default rationale as the field above.
    ///
    /// NOTE both of these are the position of the PRECEDING scheduler COMMIT,
    /// so when no COMMIT precedes the differing record they collapse to the
    /// origin and BOUND the divergence rather than locating it. The field below
    /// is the one that locates it.
    #[serde(default)]
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// 1-based index of the first differing compared record -- the LOCATION of
    /// the divergence rather than a bound on it, and the only one of the three
    /// that is a true log prefix. It shares a unit with
    /// `compared_log_messages`, so `record / compared` is the fraction of the
    /// log that was deterministic.
    ///
    /// `null` and `0` are not the same claim: null is "no divergence located",
    /// while 0 would mean the very first record differed. Nothing writes 0 --
    /// the index is 1-based. Same tolerant-default rationale as its siblings.
    #[serde(default)]
    pub first_divergent_record: Option<u64>,
    /// How many syscalls the guest had COMPLETED when the divergence appeared,
    /// from detcore's own `finish syscall #N` counter.
    ///
    /// NOT interchangeable with `first_divergent_record`: one counts guest work
    /// and the other counts compared log records, and they move at completely
    /// different rates. Measured on one real divergence: record 98, syscall 37.
    /// `null` also covers a genuine state -- a run that diverged before any
    /// syscall completed.
    #[serde(default)]
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the left execution, with the
    /// separately recorded syscall number, scheduler turn, and committed time
    /// removed. Older reports omit it and remain readable.
    #[serde(default)]
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from the right execution.
    #[serde(default)]
    pub first_divergent_right_message: Option<String>,
}

/// Accept `null` but not a MISSING field. serde maps an absent `Option` field to
/// `None` by default, which would let a report that never mentioned a comparison
/// read as a legitimate no-result. Naming a `deserialize_with` makes the field
/// required again while still admitting an explicit null.
fn present_but_nullable_comparison<'de, D>(
    deserializer: D,
) -> Result<Option<ComparisonReport>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<ComparisonReport>::deserialize(deserializer)
}

impl VerificationReport {
    #[allow(dead_code)] // path-included retained-report readers do not produce reports
    pub fn no_result() -> Self {
        Self {
            verified: false,
            bitwise_parity: false,
            verdict: Verdict::NoResult,
            no_result_reason: Some(NoResultReason::NotRun),
            infrastructure_error: None,
            comparison: None,
            compared_log_messages: None,
            dbt_counted_branches: None,
            runtime: None,
            guest_exit_code: None,
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    /// `null` is legal only when no comparison completed: either the producer
    /// reached no verdict, or it named an infrastructure error before it could
    /// compare. A null comparison beside a product verdict is contradictory
    /// and stays refused at parse.
    fn require_consistent_outcome_fields(self) -> Result<Self, String> {
        if self.comparison.is_none()
            && !matches!(
                self.verdict,
                Verdict::NoResult | Verdict::InfrastructureError
            )
        {
            return Err(format!(
                "incomplete verification report: comparison is null but verdict is {}; null is legal only for no_result or infrastructure_error",
                self.verdict
            ));
        }
        match (&self.verdict, &self.infrastructure_error) {
            (Verdict::InfrastructureError, Some(InfrastructureError::SkidOvershoot { count }))
                if *count > 0 => {}
            (Verdict::InfrastructureError, Some(InfrastructureError::SkidOvershoot { .. })) => {
                return Err(
                    "incomplete verification report: skid_overshoot count must be positive".into(),
                );
            }
            (Verdict::InfrastructureError, None) => {
                return Err(
                    "incomplete verification report: infrastructure_error verdict omitted infrastructure_error"
                        .into(),
                );
            }
            (_, Some(_)) => {
                return Err(format!(
                    "inconsistent verification report: verdict={} carries infrastructure_error",
                    self.verdict
                ));
            }
            (_, None) => {}
        }
        Ok(self)
    }

    /// Parse the complete typed receipt. A top-level JSON object is not enough:
    /// every nested field used by admission must deserialize successfully.
    #[allow(dead_code)] // this file is path-included by consumers that use one parse form
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice::<Self>(bytes)
            .map_err(|error| format!("incomplete verification report: {error}"))?
            .require_consistent_outcome_fields()
    }

    #[allow(dead_code)] // this file is path-included by consumers that use one parse form
    pub fn from_json_value(value: serde_json::Value) -> Result<Self, String> {
        serde_json::from_value::<Self>(value)
            .map_err(|error| format!("incomplete verification report: {error}"))?
            .require_consistent_outcome_fields()
    }

    /// Parse a report written by the current producer and require every field
    /// that producer promises to emit. Retained-report readers use
    /// [`Self::from_json_value`] instead so fields added after an old run remain
    /// honest absence rather than making the whole report unreadable.
    #[allow(dead_code)] // path-included readers use different parse forms
    pub fn from_current_json_value(value: serde_json::Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "incomplete verification report: expected an object".to_string())?;
        for field in [
            "verified",
            "bitwise_parity",
            "verdict",
            "infrastructure_error",
            "comparison",
            "no_result_reason",
            "compared_log_messages",
            "guest_exit_code",
            "guest_signal",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            if !object.contains_key(field) {
                return Err(format!(
                    "incomplete verification report: missing current producer field `{field}`"
                ));
            }
        }
        if let Some(comparison) = object
            .get("comparison")
            .and_then(serde_json::Value::as_object)
        {
            for field in [
                "strictness",
                "display_name",
                "compare_logs",
                "compare_io_buffers",
                "log_scope",
                "record_envelope",
                "virtualize_time",
                "strip_lines",
                "canonicalize_addresses",
                "full_trace",
                "exact_remainder",
                "stripped_prefixes",
                "canonicalizations",
                "ignore_lines",
                "skip_commit",
                "skip_detlog",
            ] {
                if !comparison.contains_key(field) {
                    return Err(format!(
                        "incomplete verification report: missing current comparison field `{field}`"
                    ));
                }
            }
        }
        let report = Self::from_json_value(value)?;
        match (&report.verdict, &report.no_result_reason) {
            (Verdict::NoResult, Some(NoResultReason::ComparisonRefused { detail }))
                if detail.trim().is_empty() =>
            {
                Err("incomplete verification report: comparison_refused omitted its detail".into())
            }
            (Verdict::NoResult, Some(_)) => Ok(report),
            (Verdict::NoResult, None) => Ok(report),
            (_, Some(_)) => Err(format!(
                "inconsistent verification report: verdict={} carries no_result_reason",
                report.verdict
            )),
            (_, None) => Ok(report),
        }
    }

    /// Prove that the invocation actually compared the canonical INFO evidence.
    /// This is separate from whether that comparison matched: a canonical
    /// divergence is a product result, while a stripped/output-only/empty-log
    /// comparison is incomplete evidence and therefore an infrastructure result.
    pub fn require_canonical_comparison(&self) -> Result<(), String> {
        let counts = self.compared_log_messages.as_ref();
        // No comparison at all is a distinct outcome from a comparison that was
        // too weak, and it is still not admissible. Naming it separately is the
        // whole point: "no verdict was recorded" is actionable against the
        // producer, while the old "unreadable report" pointed at the reader.
        let Some(comparison) = self.comparison.as_ref() else {
            return Err(format!(
                "verification recorded no comparison at all (verdict={}), so there is no canonical INFO evidence to admit",
                self.verdict
            ));
        };
        if comparison.strictness == LogCompareStrictness::Canonical
            && comparison.compare_logs
            && comparison.record_envelope.is_canonical()
            && counts.is_some_and(|counts| counts.left > 0 && counts.right > 0)
        {
            Ok(())
        } else {
            Err(format!(
                "verification did not compare canonical non-vacuous INFO evidence: strictness={} compare_logs={} record_envelope={} messages={}/{}",
                comparison.strictness,
                comparison.compare_logs,
                comparison.record_envelope.as_str(),
                counts.map_or(0, |counts| counts.left),
                counts.map_or(0, |counts| counts.right),
            ))
        }
    }

    /// Admit a green only when both the canonical comparison and the match
    /// claim agree. `verified=true` alone is intentionally insufficient: KVM's
    /// output-only fallback can report it with zero compared INFO messages.
    pub fn require_canonical_match(&self) -> Result<(), String> {
        self.require_canonical_comparison()?;
        if self.verified && self.verdict == Verdict::Matched && self.bitwise_parity {
            Ok(())
        } else {
            Err(format!(
                "canonical verification did not match: verified={} verdict={} bitwise_parity={}",
                self.verified, self.verdict, self.bitwise_parity
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The divergence position must survive the parse. It was already being
    /// emitted by hermit and already present in the retained report string, but
    /// this struct dropped it, so no cell could ever report where its
    /// divergence began.
    #[test]
    fn divergence_position_survives_the_parse() {
        let json = br#"{"verified":false,"bitwise_parity":false,"verdict":"diverged",
            "comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":180,"right":180},
            "first_divergent_scheduler_turn":4,
            "first_divergent_virtual_nanoseconds":1767225600002825515,
            "first_divergent_record":108,
            "first_divergent_syscall":37,
            "first_divergent_left_message":"INFO detcore: left event",
            "first_divergent_right_message":"INFO detcore: right event"}"#;
        let report = VerificationReport::from_json_slice(json).expect("diverged report parses");
        assert_eq!(report.first_divergent_scheduler_turn, Some(4));
        assert_eq!(
            report.first_divergent_virtual_nanoseconds,
            Some(1_767_225_600_002_825_515)
        );
        assert_eq!(report.first_divergent_record, Some(108));
        // THE FOURTH COORDINATE, asserted because it was the one that could
        // stop arriving unnoticed. Demonstrated 2026-08-25: with this line
        // absent, renaming the field so no real report could populate it left
        // all 11 tests in this module GREEN. A struct field nothing asserts is
        // indistinguishable from one nothing populates.
        assert_eq!(report.first_divergent_syscall, Some(37));
        assert_eq!(
            report.first_divergent_left_message.as_deref(),
            Some("INFO detcore: left event")
        );
        assert_eq!(
            report.first_divergent_right_message.as_deref(),
            Some("INFO detcore: right event")
        );
    }

    /// The record index is the only coordinate that LOCATES the divergence;
    /// the other two are the preceding COMMIT and merely bound it. So a report
    /// can carry a record with both bounds collapsed to the origin, and that
    /// is a MORE informative report rather than a malformed one.
    #[test]
    fn a_located_record_does_not_require_its_bounding_siblings() {
        let json = br#"{"verified":false,"bitwise_parity":false,"verdict":"diverged",
            "comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},
            "compared_log_messages":{"left":180,"right":180},
            "first_divergent_record":3}"#;
        let report = VerificationReport::from_json_slice(json).expect("report parses");
        assert_eq!(report.first_divergent_record, Some(3));
        assert_eq!(report.first_divergent_scheduler_turn, None);
        assert_eq!(report.first_divergent_virtual_nanoseconds, None);
        assert_eq!(report.first_divergent_left_message, None);
        assert_eq!(report.first_divergent_right_message, None);
        assert_eq!(report.first_divergent_syscall, None);
    }

    /// An EARLIER divergence must report a SMALLER value in both units. This is
    /// the ordering the whole metric rests on: a fix that moves the divergence
    /// later has to be visible as a larger number, and a regression as a
    /// smaller one. Both figures below were measured with `hermit log-diff
    /// --json` against one real 131-line INFO log, diverged at two different
    /// depths -- after COMMIT turn 4 and after COMMIT turn 1.
    #[test]
    fn an_earlier_divergence_reports_a_smaller_position() {
        let parse = |turn: u64, nanos: u64| {
            let json = format!(
                r#"{{"verified":false,"bitwise_parity":false,"verdict":"diverged",
                    "comparison":{{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"}},"compared_log_messages":{{"left":180,"right":180}},
                    "first_divergent_scheduler_turn":{turn},
                    "first_divergent_virtual_nanoseconds":{nanos}}}"#
            );
            VerificationReport::from_json_slice(json.as_bytes()).expect("report parses")
        };
        let late = parse(4, 1_767_225_600_002_825_515);
        let early = parse(1, 1_767_225_600_000_500_000);
        assert!(
            early.first_divergent_scheduler_turn < late.first_divergent_scheduler_turn,
            "an earlier divergence must order before a later one in scheduler turns"
        );
        assert!(
            early.first_divergent_virtual_nanoseconds < late.first_divergent_virtual_nanoseconds,
            "and in virtual nanoseconds"
        );
    }

    /// A report written before the field existed must still parse. Requiring it
    /// would reclassify every retained pre-field report as an unreadable-report
    /// infrastructure fault.
    #[test]
    fn a_report_without_the_divergence_position_still_parses() {
        let json = br#"{"verified":true,"bitwise_parity":true,"verdict":"matched",
            "comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":180,"right":180}}"#;
        let report = VerificationReport::from_json_slice(json);
        let report = report.expect("a pre-field report is not malformed");
        assert_eq!(report.first_divergent_scheduler_turn, None);
        assert_eq!(report.first_divergent_virtual_nanoseconds, None);
    }

    #[test]
    fn runtime_totals_survive_the_typed_report_parse() {
        let json = br#"{"verified":true,"bitwise_parity":true,"verdict":"matched",
            "comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1"},"compared_log_messages":{"left":180,"right":180},
            "runtime":{"run1":{"scheduler_turns":12,"virtual_nanoseconds":34,"syscalls":5},
                       "run2":{"scheduler_turns":13,"virtual_nanoseconds":35,"syscalls":6}}}"#;
        let report = VerificationReport::from_json_slice(json).expect("runtime report parses");
        let runtime = report.runtime.expect("runtime retained");
        assert_eq!(runtime.run1.expect("run1").syscalls, Some(5));
        assert_eq!(runtime.run2.expect("run2").scheduler_turns, 13);
    }

    fn report(
        strictness: LogCompareStrictness,
        compare_logs: bool,
        left: u64,
        right: u64,
    ) -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: Verdict::Matched,
            no_result_reason: None,
            infrastructure_error: None,
            comparison: Some(ComparisonReport {
                strictness,
                display_name: None,
                compare_logs,
                compare_io_buffers: None,
                log_scope: None,
                record_envelope: RecordEnvelopeReport::AllRecordsV1,
                virtualize_time: None,
                strip_lines: None,
                canonicalize_addresses: None,
                full_trace: None,
                exact_remainder: None,
                stripped_prefixes: None,
                canonicalizations: None,
                ignore_lines: None,
                skip_commit: None,
                skip_detlog: None,
            }),
            compared_log_messages: Some(ComparedLogMessages { left, right }),
            dbt_counted_branches: None,
            runtime: None,
            guest_exit_code: None,
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    #[test]
    fn brackets_every_canonical_match_requirement() {
        assert!(
            report(LogCompareStrictness::Canonical, true, 1, 1)
                .require_canonical_match()
                .is_ok()
        );
        let mut weak = report(LogCompareStrictness::Canonical, true, 1, 1);
        weak.verified = false;
        assert!(weak.require_canonical_match().is_err());
        let mut weak = report(LogCompareStrictness::Canonical, true, 1, 1);
        weak.verdict = Verdict::Diverged;
        assert!(weak.require_canonical_match().is_err());
        let mut weak = report(LogCompareStrictness::Canonical, true, 1, 1);
        weak.bitwise_parity = false;
        assert!(weak.require_canonical_match().is_err());
        assert!(
            report(LogCompareStrictness::Stripped, true, 1, 1)
                .require_canonical_match()
                .is_err()
        );
        assert!(
            report(LogCompareStrictness::Canonical, false, 1, 1)
                .require_canonical_match()
                .is_err()
        );
        assert!(
            report(LogCompareStrictness::Canonical, true, 0, 1)
                .require_canonical_match()
                .is_err()
        );
        let mut weak = report(LogCompareStrictness::Canonical, true, 1, 1);
        weak.comparison.as_mut().unwrap().record_envelope = RecordEnvelopeReport::CallerDefined;
        assert!(weak.require_canonical_match().is_err());
    }

    /// The exact bytes hermit writes for a run that reached no verdict, copied
    /// from a real capture rather than reconstructed. Before this was `Option`,
    /// parsing these failed with "invalid type: null, expected struct
    /// ComparisonReport at line 1 column 80" and the cell was scored ERROR
    /// "verification report is unreadable" -- an infrastructure fault where the
    /// truth was that the producer had recorded nothing.
    const REAL_NO_RESULT_REPORT: &str = concat!(
        r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","#,
        r#""comparison":null,"compared_log_messages":null,"guest_exit_code":null,"#,
        r#""guest_signal":null,"first_divergent_scheduler_turn":null,"#,
        r#""first_divergent_virtual_nanoseconds":null,"first_divergent_record":null,"#,
        r#""first_divergent_syscall":null}"#,
    );

    #[test]
    fn a_real_no_result_report_parses_instead_of_reading_as_unreadable() {
        let parsed = VerificationReport::from_json_slice(REAL_NO_RESULT_REPORT.as_bytes())
            .expect("a documented no-result report must parse");
        assert_eq!(parsed.verdict, Verdict::NoResult);
        assert!(parsed.comparison.is_none());
    }

    #[test]
    fn a_no_result_report_is_still_inadmissible_and_names_the_real_reason() {
        // Parsing it is NOT admitting it. The point of the change is that the
        // refusal now points at the producer that recorded nothing, instead of
        // claiming the report could not be read.
        let parsed = VerificationReport::from_json_slice(REAL_NO_RESULT_REPORT.as_bytes())
            .expect("a documented no-result report must parse");
        let refusal = parsed
            .require_canonical_comparison()
            .expect_err("a report with no comparison must never be admitted");
        assert!(
            refusal.contains("recorded no comparison at all"),
            "refusal must name the missing comparison, got: {refusal}"
        );
        assert!(
            refusal.contains("no_result"),
            "refusal must carry the verdict, got: {refusal}"
        );
        assert!(parsed.require_canonical_match().is_err());
    }

    #[test]
    fn a_missing_comparison_field_is_refused_rather_than_defaulting_to_none() {
        // serde would otherwise map an absent Option field to None, letting a
        // report that never mentioned a comparison read as a legitimate
        // no-result. Optional must not mean absent.
        let absent = r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result","compared_log_messages":null}"#;
        assert!(VerificationReport::from_json_slice(absent.as_bytes()).is_err());
    }

    #[test]
    fn a_malformed_comparison_object_is_still_refused_at_parse() {
        // Optional must not mean lenient: a comparison that is present but the
        // wrong shape is still an unreadable report, not a silent None.
        let malformed = REAL_NO_RESULT_REPORT.replace(r#""comparison":null"#, r#""comparison":7"#);
        assert!(VerificationReport::from_json_slice(malformed.as_bytes()).is_err());
        let missing_field = REAL_NO_RESULT_REPORT.replace(
            r#""comparison":null"#,
            r#""comparison":{"compare_logs":true,"record_envelope":"all_records_v1"}"#,
        );
        assert!(VerificationReport::from_json_slice(missing_field.as_bytes()).is_err());
    }

    #[test]
    fn comparison_record_envelope_is_required_and_unknown_values_are_refused() {
        let missing = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "infrastructure_error": null,
            "comparison": {"strictness": "canonical", "compare_logs": true},
            "compared_log_messages": {"left": 1, "right": 1}
        });
        let error = VerificationReport::from_json_value(missing)
            .expect_err("a comparison that never states its record envelope is not admissible")
            .to_string();
        // The runner surfaces this verbatim as "verification report is
        // unreadable: {error}". Refusing is only useful if the operator can
        // tell WHICH producer contract was unmet -- an old binary that predates
        // the field looks identical to a corrupt file unless the message names
        // it. Assert the naming rather than trusting serde to keep doing it.
        assert!(
            error.contains("record_envelope"),
            "the refusal must name the missing field so an operator can tell a producer that \
             predates the envelope from a corrupt report; got: {error}"
        );

        let unknown = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "infrastructure_error": null,
            "comparison": {
                "strictness": "canonical",
                "compare_logs": true,
                "record_envelope": "unknown_v9"
            },
            "compared_log_messages": {"left": 1, "right": 1}
        });
        assert!(VerificationReport::from_json_value(unknown).is_err());
    }

    #[test]
    fn readable_object_with_unreadable_nested_predicate_is_refused() {
        let malformed = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "comparison": null,
            "compared_log_messages": {"left": 1, "right": 1}
        });
        assert!(VerificationReport::from_json_value(malformed).is_err());
    }

    #[test]
    fn typed_no_result_reason_round_trips_and_unknown_values_are_refused() {
        let current = serde_json::to_value(VerificationReport::no_result()).unwrap();
        let parsed = VerificationReport::from_current_json_value(current.clone())
            .expect("the typed not-run reason must round-trip");
        assert_eq!(parsed.no_result_reason, Some(NoResultReason::NotRun));

        let mut missing = current.clone();
        missing.as_object_mut().unwrap().remove("no_result_reason");
        let error = VerificationReport::from_current_json_value(missing)
            .expect_err("a current no-result must state the nullable reason field");
        assert!(error.contains("no_result_reason"), "{error}");

        let mut absent = current.clone();
        absent["no_result_reason"] = serde_json::Value::Null;
        let parsed = VerificationReport::from_current_json_value(absent)
            .expect("explicit null records that the producer has no exact cause");
        assert_eq!(parsed.no_result_reason, None);

        let mut comparator_refusal = current.clone();
        comparator_refusal["no_result_reason"] = serde_json::json!({
            "kind": "comparison_refused",
            "detail": "the first log was truncated"
        });
        let parsed = VerificationReport::from_current_json_value(comparator_refusal)
            .expect("a comparator refusal carries the producer's exact cause");
        assert_eq!(
            parsed.no_result_reason,
            Some(NoResultReason::ComparisonRefused {
                detail: "the first log was truncated".into()
            })
        );

        let mut unknown = current;
        unknown["no_result_reason"] = serde_json::json!({"kind": "never_ran"});
        let error = VerificationReport::from_current_json_value(unknown)
            .expect_err("an unknown no-result reason must be refused");
        assert!(
            error.contains("unknown variant"),
            "refusal must identify the unknown typed reason: {error}"
        );
    }

    #[test]
    fn current_comparison_requires_every_producer_field_by_name() {
        let mut current = serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "no_result_reason": null,
            "infrastructure_error": null,
            "comparison": {
                "strictness": "canonical",
                "display_name": "BitwiseInfoV1",
                "compare_logs": true,
                "compare_io_buffers": true,
                "log_scope": "info",
                "record_envelope": "all_records_v1",
                "virtualize_time": true,
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
            "compared_log_messages": {"left": 1, "right": 1},
            "guest_exit_code": 0,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        });
        let parsed = VerificationReport::from_current_json_value(current.clone())
            .expect("the complete current producer shape must parse");
        let comparison = parsed.comparison.expect("matched report has comparison");
        assert_eq!(comparison.compare_io_buffers, Some(true));
        assert_eq!(comparison.log_scope, Some(ComparedLogScope::Info));
        assert_eq!(parsed.guest_exit_code, Some(0));
        current["comparison"]
            .as_object_mut()
            .unwrap()
            .remove("compare_io_buffers");
        let error = VerificationReport::from_current_json_value(current)
            .expect_err("current comparison missing a producer field must be refused");
        assert!(
            error.contains("compare_io_buffers"),
            "refusal must name the missing comparison field: {error}"
        );
    }

    #[test]
    fn unknown_verdict_is_refused_by_the_shared_type() {
        let unknown = serde_json::json!({
            "verified": false,
            "bitwise_parity": false,
            "verdict": "partly_matched",
            "comparison": null,
            "compared_log_messages": null
        });
        let error = VerificationReport::from_json_value(unknown)
            .expect_err("an unknown verdict must not become a product failure");
        assert!(
            error.contains("unknown variant"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn infrastructure_error_is_distinct_from_no_result_and_requires_a_positive_count() {
        let mut report =
            serde_json::to_value(report(LogCompareStrictness::Canonical, true, 1, 1)).unwrap();
        report["verified"] = serde_json::json!(false);
        report["bitwise_parity"] = serde_json::json!(false);
        report["verdict"] = serde_json::json!("infrastructure_error");
        report["infrastructure_error"] = serde_json::json!({"kind": "skid_overshoot", "count": 2});

        let parsed = VerificationReport::from_json_value(report.clone()).unwrap();
        assert_eq!(parsed.verdict, Verdict::InfrastructureError);
        assert_eq!(
            parsed.infrastructure_error,
            Some(InfrastructureError::SkidOvershoot { count: 2 })
        );
        assert!(parsed.comparison.is_some());

        report["infrastructure_error"]["count"] = serde_json::json!(0);
        let error = VerificationReport::from_json_value(report)
            .expect_err("zero cannot describe an observed skid overshoot");
        assert!(error.contains("count must be positive"), "{error}");
    }
}
