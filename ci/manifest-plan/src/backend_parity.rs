//! Typed evidence for one candidate backend compared with the ptrace reference.
//!
//! Each operand has already passed its own ordinary two-execution strict
//! verification.  The cross-backend comparison then uses one retained run from
//! each operand.  This keeps repeatability and backend parity as separate facts
//! while letting one cell carry both without an after-the-fact row join.

use serde::Deserialize;
use serde::Serialize;

use crate::canonical_verdict::ComparedOutput;
use crate::canonical_verdict::VerificationReport;
use crate::logdiff_report::LogDiffReport;
use crate::logdiff_report::LogDiffVerdict;

pub const BACKEND_PARITY_REPORT_SCHEMA: u64 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendParityVerdict {
    Matched,
    Diverged,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendParityOperand {
    pub backend: String,
    pub verification: VerificationReport,
    pub output: ComparedOutput,
    pub retained_log: String,
    pub retained_log_sha256: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendParityReport {
    pub schema: u64,
    pub verdict: BackendParityVerdict,
    pub reference: BackendParityOperand,
    pub candidate: BackendParityOperand,
    pub comparison: LogDiffReport,
}

impl BackendParityReport {
    /// Name the exact components which differ, without changing the verdict.
    pub fn differences(&self) -> Vec<&'static str> {
        let mut differences = Vec::new();
        let reference = &self.reference.output;
        let candidate = &self.candidate.output;
        if reference.exit_code != candidate.exit_code || reference.signal != candidate.signal {
            differences.push("guest exit status or signal");
        }
        if reference.stdout_sha256 != candidate.stdout_sha256
            || reference.stdout_bytes != candidate.stdout_bytes
        {
            differences.push("guest stdout");
        }
        if reference.stderr_sha256 != candidate.stderr_sha256
            || reference.stderr_bytes != candidate.stderr_bytes
        {
            differences.push("guest stderr");
        }
        if self.comparison.verdict == LogDiffVerdict::Diverged {
            differences.push("shared Detcore INFO records");
        }
        differences
    }

    /// Validate all facts needed to turn this report into a scorecard verdict.
    ///
    /// The two same-backend reports must each be strict, non-vacuous matches;
    /// this report cannot hide an internally nondeterministic candidate behind
    /// an accidentally matching cross-backend sample.  The cross comparison is
    /// independently required to be strict, complete shared-Detcore evidence.
    pub fn validate(&self, candidate_backend: &str) -> Result<(), String> {
        if self.schema != BACKEND_PARITY_REPORT_SCHEMA {
            return Err(format!(
                "backend parity report schema must be {BACKEND_PARITY_REPORT_SCHEMA}, got {}",
                self.schema
            ));
        }
        if self.reference.backend != "ptrace" {
            return Err(format!(
                "backend parity reference must be ptrace, got {:?}",
                self.reference.backend
            ));
        }
        if candidate_backend == "ptrace" || self.candidate.backend != candidate_backend {
            return Err(format!(
                "backend parity candidate must be the non-ptrace cell backend {candidate_backend:?}, got {:?}",
                self.candidate.backend
            ));
        }
        validate_operand("reference", &self.reference)?;
        validate_operand("candidate", &self.candidate)?;
        self.comparison.require_cross_backend_evidence()?;
        let inputs = self.comparison.inputs.as_ref().expect("required above");
        if inputs.left.sha256 != self.reference.retained_log_sha256
            || inputs.right.sha256 != self.candidate.retained_log_sha256
        {
            return Err(
                "backend parity retained logs differ from the comparator's captured inputs".into(),
            );
        }

        let outputs_match = self.reference.output == self.candidate.output;
        let logs_match = self.comparison.verdict == LogDiffVerdict::Matched;
        let expected = if outputs_match && logs_match {
            BackendParityVerdict::Matched
        } else {
            BackendParityVerdict::Diverged
        };
        if self.verdict != expected {
            return Err(format!(
                "backend parity verdict {:?} contradicts compared stdout, disposition, or shared Detcore records (expected {expected:?})",
                self.verdict
            ));
        }
        Ok(())
    }
}

fn validate_operand(label: &str, operand: &BackendParityOperand) -> Result<(), String> {
    operand
        .verification
        .require_canonical_match()
        .map_err(|error| format!("{label} same-backend verification is not canonical: {error}"))?;
    let compared_outputs = operand
        .verification
        .compared_outputs
        .as_ref()
        .ok_or_else(|| {
            format!("{label} same-backend verification omitted exact output evidence")
        })?;
    compared_outputs
        .require_exact_match()
        .map_err(|error| format!("{label} same-backend output comparison failed: {error}"))?;
    if operand.output != compared_outputs.left {
        return Err(format!(
            "{label} selected output does not match its same-backend verification operand"
        ));
    }
    if operand.verification.guest_exit_code != operand.output.exit_code
        || operand.verification.guest_signal != operand.output.signal
    {
        return Err(format!(
            "{label} selected output disposition contradicts its verification result"
        ));
    }
    for (field, digest) in [
        ("stdout_sha256", operand.output.stdout_sha256.as_str()),
        ("stderr_sha256", operand.output.stderr_sha256.as_str()),
        ("retained_log_sha256", operand.retained_log_sha256.as_str()),
    ] {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!("{label} {field} is not a SHA-256 digest"));
        }
    }
    if operand.retained_log.trim().is_empty() {
        return Err(format!("{label} retained_log is empty"));
    }
    if operand.output.exit_code.is_some() == operand.output.signal.is_some() {
        return Err(format!(
            "{label} must record exactly one guest disposition (exit code or signal)"
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::canonical_verdict::ComparedLogMessages;
    use crate::canonical_verdict::ComparedLogScope;
    use crate::canonical_verdict::ComparedOutputs;
    use crate::canonical_verdict::ComparisonReport;
    use crate::canonical_verdict::LogCompareStrictness;
    use crate::canonical_verdict::RecordEnvelopeReport;
    use crate::canonical_verdict::Verdict;
    use crate::logdiff_report::LOG_DIFF_REPORT_SCHEMA;
    use crate::logdiff_report::LogDiffComparison;
    use crate::logdiff_report::LogDiffMessageCounts;
    use crate::logdiff_report::LogDiffRecords;
    use crate::logdiff_report::RecordEnvelopePolicy;

    fn output(stdout: char) -> ComparedOutput {
        ComparedOutput {
            exit_code: Some(0),
            signal: None,
            stdout_sha256: stdout.to_string().repeat(64),
            stdout_bytes: 4,
            stderr_sha256: "d".repeat(64),
            stderr_bytes: 0,
        }
    }

    fn verification(output: ComparedOutput) -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: Verdict::Matched,
            no_result_reason: None,
            infrastructure_error: None,
            comparison: Some(ComparisonReport {
                strictness: LogCompareStrictness::Canonical,
                display_name: Some("BitwiseInfoV1".into()),
                compare_logs: true,
                compare_io_buffers: Some(true),
                log_scope: Some(ComparedLogScope::Info),
                record_envelope: RecordEnvelopeReport::AllRecordsV1,
                virtualize_time: Some(true),
                strip_lines: Some(false),
                canonicalize_addresses: Some(true),
                full_trace: Some(true),
                exact_remainder: Some(true),
                stripped_prefixes: Some(vec!["real-wall-clock-prefix/v1".into()]),
                canonicalizations: Some(vec!["host-address-to-first-appearance-ordinal/v1".into()]),
                ignore_lines: Some(false),
                skip_commit: Some(false),
                skip_detlog: Some(false),
            }),
            compared_log_messages: Some(ComparedLogMessages { left: 2, right: 2 }),
            compared_outputs: Some(ComparedOutputs {
                left: output.clone(),
                right: output,
            }),
            dbt_counted_branches: None,
            runtime: None,
            guest_exit_code: Some(0),
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    pub(crate) fn report(verdict: BackendParityVerdict) -> BackendParityReport {
        let output = output('a');
        BackendParityReport {
            schema: BACKEND_PARITY_REPORT_SCHEMA,
            verdict,
            reference: BackendParityOperand {
                backend: "ptrace".into(),
                verification: verification(output.clone()),
                output: output.clone(),
                retained_log: "reference/run1.log".into(),
                retained_log_sha256: "b".repeat(64),
            },
            candidate: BackendParityOperand {
                backend: "kvm".into(),
                verification: verification(output.clone()),
                output,
                retained_log: "candidate/run1.log".into(),
                retained_log_sha256: "c".repeat(64),
            },
            comparison: LogDiffReport {
                schema: LOG_DIFF_REPORT_SCHEMA,
                verdict: match verdict {
                    BackendParityVerdict::Matched => LogDiffVerdict::Matched,
                    BackendParityVerdict::Diverged => LogDiffVerdict::Diverged,
                },
                refusal: None,
                inputs: Some(crate::logdiff_report::LogDiffInputs {
                    left: crate::logdiff_report::LogDiffInput {
                        sha256: "b".repeat(64),
                        bytes: 21,
                    },
                    right: crate::logdiff_report::LogDiffInput {
                        sha256: "c".repeat(64),
                        bytes: 21,
                    },
                }),
                selected_messages: LogDiffMessageCounts { left: 2, right: 2 },
                records: LogDiffRecords {
                    compared: 2,
                    available_left: 2,
                    available_right: 2,
                    withheld_incomplete_tail: false,
                },
                comparison: LogDiffComparison {
                    stream: "info".into(),
                    record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
                    unsafe_strip_lines: false,
                    canonicalize_host_addresses: true,
                    require_structured_events: true,
                    ignored_line_substrings: Vec::new(),
                    skip_commit: false,
                    skip_detlog: false,
                    included_detlog_kinds: vec![
                        "syscall".into(),
                        "syscall_result".into(),
                        "other".into(),
                    ],
                    git_diff: false,
                },
                follow_stopped_because: None,
                first_divergent_record: (verdict == BackendParityVerdict::Diverged).then_some(2),
                first_divergent_syscall: None,
                first_divergent_scheduler_turn: None,
                first_divergent_virtual_nanoseconds: None,
                first_divergent_left_message: None,
                first_divergent_right_message: None,
            },
        }
    }

    #[test]
    fn matching_and_divergent_reports_validate_both_directions() {
        report(BackendParityVerdict::Matched)
            .validate("kvm")
            .unwrap();
        report(BackendParityVerdict::Diverged)
            .validate("kvm")
            .unwrap();
    }

    #[test]
    fn output_mismatch_cannot_be_reported_as_a_match() {
        let mut report = report(BackendParityVerdict::Matched);
        let different = output('e');
        report.candidate.output = different.clone();
        report.candidate.verification.compared_outputs = Some(ComparedOutputs {
            left: different.clone(),
            right: different,
        });
        assert!(report.validate("kvm").is_err());
        report.verdict = BackendParityVerdict::Diverged;
        report.validate("kvm").unwrap();
        assert_eq!(report.differences(), ["guest stdout"]);
    }

    #[test]
    fn disposition_mismatch_is_a_real_cross_backend_divergence() {
        let mut report = report(BackendParityVerdict::Matched);
        let mut different = output('a');
        different.exit_code = Some(7);
        report.candidate.output = different.clone();
        report.candidate.verification.compared_outputs = Some(ComparedOutputs {
            left: different.clone(),
            right: different,
        });
        report.candidate.verification.guest_exit_code = Some(7);
        assert!(report.validate("kvm").is_err());
        report.verdict = BackendParityVerdict::Diverged;
        report.validate("kvm").unwrap();
        assert_eq!(report.differences(), ["guest exit status or signal"]);
    }

    #[test]
    fn empty_or_relaxed_log_evidence_is_refused() {
        let mut empty_report = report(BackendParityVerdict::Matched);
        empty_report.comparison.records.compared = 0;
        assert!(empty_report.validate("kvm").is_err());

        let mut relaxed_report = report(BackendParityVerdict::Matched);
        relaxed_report.comparison.comparison.skip_detlog = true;
        assert!(relaxed_report.validate("kvm").is_err());

        let mut missing_inputs = report(BackendParityVerdict::Matched);
        missing_inputs.comparison.inputs = None;
        // Historical reports remain readable, but absence cannot become parity.
        let retained: BackendParityReport =
            serde_json::from_slice(&serde_json::to_vec(&missing_inputs).unwrap()).unwrap();
        assert!(
            retained
                .validate("kvm")
                .unwrap_err()
                .contains("captured input")
        );
        for reference in [true, false] {
            let mut replaced = report(BackendParityVerdict::Matched);
            if reference {
                replaced.reference.retained_log_sha256 = "e".repeat(64);
            } else {
                replaced.candidate.retained_log_sha256 = "e".repeat(64);
            }
            assert!(
                replaced
                    .validate("kvm")
                    .unwrap_err()
                    .contains("captured inputs")
            );
        }
        for (digest, bytes) in [("z".repeat(64), 21), ("b".repeat(64), 0)] {
            let mut invalid = report(BackendParityVerdict::Matched);
            let input = &mut invalid.comparison.inputs.as_mut().unwrap().left;
            input.sha256 = digest;
            input.bytes = bytes;
            assert!(invalid.validate("kvm").is_err());
        }
    }

    #[test]
    fn parity_counts_require_complete_inputs_and_consistent_selected_streams() {
        let mut complete = report(BackendParityVerdict::Matched);
        complete.comparison.records.available_left = 4;
        complete.comparison.records.available_right = 7;
        complete.comparison.records.compared = 4;
        complete.validate("kvm").unwrap();

        for (compared, left, right) in [(3, 2, 2), (5, 2, 2), (4, 2, 3), (4, 5, 5)] {
            let mut invalid = complete.clone();
            invalid.comparison.records.compared = compared;
            invalid.comparison.selected_messages = LogDiffMessageCounts { left, right };
            assert!(
                invalid.validate("kvm").is_err(),
                "{compared}/{left}/{right}"
            );
        }

        // A completed divergence can describe different selected lengths.
        complete.verdict = BackendParityVerdict::Diverged;
        complete.comparison.verdict = LogDiffVerdict::Diverged;
        complete.comparison.selected_messages.right = 3;
        complete.validate("kvm").unwrap();
    }
}
