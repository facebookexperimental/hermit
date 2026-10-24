//! Shared interpretation of one producer-written raw cell's ordinary comparison.
//! This is the unchanged validator policy, reused by public evidence derivation.

use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use super::CellVerdict;
use super::ComparedLogCounts;
use super::ComparisonSpec;
use super::ComparisonTier;
use super::RequiredNullable;
use crate::canonical_verdict::InfrastructureError;
use crate::canonical_verdict::Verdict as VerificationVerdict;
use crate::canonical_verdict::VerificationReport;

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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
            Some(InfrastructureError::SkidOvershoot { count }) => {
                format!("recorded infrastructure_error: {count} HERMIT_SKID_OVERSHOOT report(s)")
            }
            Some(change @ InfrastructureError::HostInputChanged { .. }) => {
                format!("recorded infrastructure_error: {change}")
            }
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

/// Whether the producer row declares the stripped comparator, by the runner's
/// own rule for its recorded relaxations.
fn declares_stripped_comparator(row: &Value) -> bool {
    let relaxations: Vec<String> = row
        .get("relaxations")
        .and_then(Value::as_array)
        .map(|relaxations| {
            relaxations
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    crate::runner::records_stripped_comparator(&relaxations)
}

/// The stripped comparison one attempt's report must hold: Hermit's stripped
/// log comparison with virtual time, over a non-empty event stream on both
/// runs. `None` when the report is not that comparison.
fn stripped_report(
    value: Value,
    bytes: &[u8],
) -> Result<Option<(VerificationReport, ComparisonSpec, ComparedLogCounts)>, String> {
    let report = VerificationReport::from_current_json_slice(bytes)?;
    if report.verdict == VerificationVerdict::InfrastructureError {
        return Err("recorded infrastructure_error".into());
    }
    let Some(comparison) = value.get("comparison").cloned() else {
        return Ok(None);
    };
    let comparison = serde_json::from_value::<ComparisonSpec>(comparison)
        .map_err(|error| format!("incomplete cell comparison: {error}"))?;
    let counts = serde_json::from_value::<RequiredNullable<ComparedLogCounts>>(
        value
            .get("compared_log_messages")
            .cloned()
            .ok_or("incomplete cell comparison: missing `compared_log_messages`")?,
    )
    .map_err(|error| format!("incomplete cell comparison counts: {error}"))?;
    if !comparison.is_stripped_verify_comparison(&counts) {
        return Ok(None);
    }
    let RequiredNullable::Value(counts) = counts else {
        return Ok(None);
    };
    Ok(Some((report, comparison, counts)))
}

/// The verdict of a verify cell that declared the stripped comparator.
///
/// Its evidence is a real two-run comparison of exit status, streams and
/// stripped logs, below the canonical L2 comparison, so a match is
/// `ComparedAndMatched` at tier `ExitAndStreamEquality` with
/// `bitwise_parity: false` and never canonical evidence; a stripped
/// divergence is `ComparedAndDiverged` at the same tier. Anything weaker than
/// the declared stripped comparison is unavailable, exactly as for a
/// canonical cell.
fn stripped_cell_verdict(row: &Value) -> Result<CellVerdict, String> {
    let unavailable = |reason: String| CellVerdict::UnavailableWithReason {
        comparison_tier: ComparisonTier::DeclaredButUnverifiable,
        reason,
    };
    let Some(attempts) = row.get("attempts").and_then(Value::as_array) else {
        return Ok(unavailable(
            preserved_reason(row, None)
                .unwrap_or("cell emitted no typed attempts")
                .into(),
        ));
    };
    let mut reports = Vec::new();
    let mut unavailable_reason = None;
    for (index, attempt) in attempts.iter().enumerate() {
        let preserved = preserved_reason(row, Some(attempt)).map(str::to_owned);
        let Some(raw) = attempt.get("verification_report").and_then(Value::as_str) else {
            unavailable_reason = Some(preserved.unwrap_or_else(|| {
                format!("attempt {} emitted no typed verification report", index + 1)
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
        match stripped_report(value, raw.as_bytes()) {
            Ok(Some(report)) => reports.push(report),
            Ok(None) => {
                unavailable_reason = Some(preserved.unwrap_or_else(|| {
                    format!(
                        "attempt {} did not compare non-vacuous stripped evidence",
                        index + 1
                    )
                }));
            }
            Err(error) => {
                unavailable_reason =
                    Some(preserved.unwrap_or_else(|| format!("attempt {} {error}", index + 1)))
            }
        }
    }
    let compared = |state_matched: bool,
                    comparison: &ComparisonSpec,
                    counts: &ComparedLogCounts|
     -> CellVerdict {
        let counts = RequiredNullable::Value(counts.clone());
        if state_matched {
            CellVerdict::ComparedAndMatched {
                comparison_tier: ComparisonTier::ExitAndStreamEquality,
                comparison: comparison.clone(),
                bitwise_parity: false,
                compared_log_messages: counts,
            }
        } else {
            CellVerdict::ComparedAndDiverged {
                comparison_tier: ComparisonTier::ExitAndStreamEquality,
                comparison: comparison.clone(),
                bitwise_parity: false,
                compared_log_messages: counts,
            }
        }
    };
    // A divergence is sticky across sibling attempts, as for canonical cells.
    if let Some((_, comparison, counts)) = reports
        .iter()
        .find(|(report, _, _)| report.verdict == VerificationVerdict::Diverged)
    {
        return Ok(compared(false, comparison, counts));
    }
    if reports.is_empty() || unavailable_reason.is_some() {
        return Ok(unavailable(unavailable_reason.unwrap_or_else(|| {
            "cell emitted no typed verification report".into()
        })));
    }
    if reports
        .iter()
        .any(|(report, _, _)| !(report.verified && report.verdict == VerificationVerdict::Matched))
    {
        return Ok(unavailable(
            "typed stripped report was neither a match nor a divergence".into(),
        ));
    }
    if string(row, "outcome")? != "PASS" {
        return Ok(unavailable(
            preserved_reason(row, None)
                .unwrap_or("cell outcome was not PASS despite matched stripped comparison")
                .into(),
        ));
    }
    let (_, comparison, counts) = reports.last().expect("nonempty reports");
    Ok(compared(true, comparison, counts))
}

pub fn cell_verdict_from_source(row: &Value) -> Result<CellVerdict, String> {
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
    // The runner accepts the stripped comparator only in verify mode.
    if mode == "verify" && declares_stripped_comparator(row) {
        return stripped_cell_verdict(row);
    }
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
                format!("attempt {} emitted no typed verification report", index + 1)
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

#[cfg(test)]
mod stripped_tests {
    use serde_json::json;

    use super::*;
    use crate::runner::PRODUCER_STRIPPED_REPORT as STRIPPED_REPORT;

    const STRIPPED: &str =
        "comparator=stripped: the corpus verdict policy is the stripped comparison";

    fn row(outcome: &str, relaxations: &[&str], reports: &[String]) -> Value {
        let attempts: Vec<Value> = reports
            .iter()
            .map(|report| {
                json!({
                    "verification_report": report,
                    "verification_report_sha256": hex_digest(report.as_bytes()),
                })
            })
            .collect();
        json!({
            "mode": "verify",
            "outcome": outcome,
            "relaxations": relaxations,
            "attempts": attempts,
        })
    }

    fn diverged() -> String {
        STRIPPED_REPORT
            .replace(r#""verified":true"#, r#""verified":false"#)
            .replace(r#""verdict":"matched""#, r#""verdict":"diverged""#)
    }

    #[test]
    fn a_declared_stripped_match_is_a_weak_tier_match_never_canonical() {
        let verdict =
            cell_verdict_from_source(&row("PASS", &[STRIPPED], &[STRIPPED_REPORT.into()])).unwrap();
        let CellVerdict::ComparedAndMatched {
            comparison_tier,
            comparison,
            bitwise_parity,
            compared_log_messages,
        } = verdict
        else {
            panic!("expected a stripped match, got {verdict:?}");
        };
        assert_eq!(comparison_tier, ComparisonTier::ExitAndStreamEquality);
        assert!(!bitwise_parity);
        assert_eq!(
            comparison.strictness,
            super::super::ComparisonStrictness::Stripped
        );
        assert_eq!(
            compared_log_messages,
            RequiredNullable::Value(ComparedLogCounts {
                left: 243,
                right: 243
            })
        );
    }

    #[test]
    fn a_stripped_divergence_is_sticky_and_stays_at_the_weak_tier() {
        for reports in [
            vec![diverged()],
            vec![diverged(), STRIPPED_REPORT.to_string()],
            vec![STRIPPED_REPORT.to_string(), diverged()],
        ] {
            let verdict = cell_verdict_from_source(&row("PASS", &[STRIPPED], &reports)).unwrap();
            assert!(
                matches!(
                    verdict,
                    CellVerdict::ComparedAndDiverged {
                        comparison_tier: ComparisonTier::ExitAndStreamEquality,
                        bitwise_parity: false,
                        ..
                    }
                ),
                "{verdict:?}"
            );
        }
    }

    #[test]
    fn only_a_declared_complete_stripped_pass_is_a_match() {
        let unavailable = |row: Value| {
            let verdict = cell_verdict_from_source(&row).unwrap();
            assert!(
                matches!(
                    verdict,
                    CellVerdict::UnavailableWithReason {
                        comparison_tier: ComparisonTier::DeclaredButUnverifiable,
                        ..
                    }
                ),
                "{verdict:?}"
            );
        };
        // Undeclared: the canonical path refuses a stripped report, as before.
        unavailable(row("PASS", &[], &[STRIPPED_REPORT.into()]));
        // A blank reason or two declarations do not declare it.
        unavailable(row(
            "PASS",
            &["comparator=stripped:  "],
            &[STRIPPED_REPORT.into()],
        ));
        unavailable(row(
            "PASS",
            &[STRIPPED, STRIPPED],
            &[STRIPPED_REPORT.into()],
        ));
        // A matched comparison with a non-PASS outcome.
        unavailable(row("FAIL", &[STRIPPED], &[STRIPPED_REPORT.into()]));
        // Vacuous evidence: no compared events on one side.
        unavailable(row(
            "PASS",
            &[STRIPPED],
            &[STRIPPED_REPORT.replace(r#""left":243"#, r#""left":0"#)],
        ));
        // Logs not compared, or real time.
        unavailable(row(
            "PASS",
            &[STRIPPED],
            &[STRIPPED_REPORT.replace(r#""compare_logs":true"#, r#""compare_logs":false"#)],
        ));
        unavailable(row(
            "PASS",
            &[STRIPPED],
            &[STRIPPED_REPORT.replace(r#""virtualize_time":true"#, r#""virtualize_time":false"#)],
        ));
        // A declared stripped cell whose report is not stripped.
        unavailable(row(
            "PASS",
            &[STRIPPED],
            &[
                STRIPPED_REPORT
                    .replace(r#""strictness":"stripped""#, r#""strictness":"canonical""#),
            ],
        ));
        // No report at all.
        unavailable(row("PASS", &[STRIPPED], &[]));
        // A replay row is never judged by the stripped comparator.
        let mut replay = row("PASS", &[STRIPPED], &[STRIPPED_REPORT.into()]);
        replay["mode"] = "replay".into();
        unavailable(replay);
    }
}
