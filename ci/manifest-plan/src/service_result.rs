//! Serialized terminal result written by the validation framework.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

pub const HISTORICAL_SCHEMA_VERSION: u64 = 1;
pub const WRITEBACK_SCHEMA_VERSION: u64 = 2;
pub const SELECTION_SCHEMA_VERSION: u64 = 3;
pub const TEST_COUNTS_SCHEMA_VERSION: u64 = 4;
pub const DETAIL_SCHEMA_VERSION: u64 = 5;
/// Schema 6 keeps the schema-5 field set and adds the `delegated` scorecard
/// write-back variant, so a run whose publication belongs to the launcher says
/// so explicitly instead of writing an ambiguous null.
pub const DIAGNOSTIC_COUNTS_SCHEMA_VERSION: u64 = 7;
/// Schema 7 adds `diagnostic_failed_tests`: executed tests their producer
/// declared non-blocking diagnostics (schema-4 `diagnostic_fail` rows). They
/// are neither passes nor blocking failures, so a passing result must account
/// for them explicitly: `passed + diagnostic == executed`. Schemas before 7
/// cannot carry the field and keep the exact `passed == executed` rule.
pub const SCHEMA_VERSION: u64 = 7;
pub const HISTORICAL_FIELD_NAMES: [&str; 7] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
];
pub const WRITEBACK_FIELD_NAMES: [&str; 8] = [
    "schema_version",
    "commit",
    "profile",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "scorecard_writeback",
];
pub const SELECTION_FIELD_NAMES: [&str; 9] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "scorecard_writeback",
];
pub const TEST_COUNTS_FIELD_NAMES: [&str; 10] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "passed_tests",
    "scorecard_writeback",
];
pub const FIELD_NAMES: [&str; 11] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "detail",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "passed_tests",
    "scorecard_writeback",
];
pub const DIAGNOSTIC_COUNTS_FIELD_NAMES: [&str; 12] = [
    "schema_version",
    "commit",
    "profile",
    "selection_mode",
    "final_validate_status",
    "detail",
    "exit_code",
    "executed_nodes",
    "executed_tests",
    "passed_tests",
    "diagnostic_failed_tests",
    "scorecard_writeback",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FinalValidateStatus {
    Passed,
    /// All tests passed, but at least one needed a bounded retry. A distinct
    /// passing status (owner lexicon 2026-10-30: GREEN-FLAKY is landable),
    /// never FAILED; the retry counts ride in the ledger rollup.
    #[serde(rename = "GREEN_FLAKY")]
    PassedFlaky,
    Failed,
    CouldNotRun,
}

impl FinalValidateStatus {
    pub const ALL: [Self; 4] = [
        Self::Passed,
        Self::PassedFlaky,
        Self::Failed,
        Self::CouldNotRun,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "PASSED",
            Self::PassedFlaky => "GREEN_FLAKY",
            Self::Failed => "FAILED",
            Self::CouldNotRun => "COULD_NOT_RUN",
        }
    }

    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Passed | Self::PassedFlaky => 0,
            Self::Failed => 1,
            Self::CouldNotRun => 75,
        }
    }

    pub const fn service_state(self) -> &'static str {
        "completed"
    }

    pub const fn service_result(self) -> &'static str {
        match self {
            Self::Passed | Self::PassedFlaky => "success",
            Self::Failed => "failure",
            Self::CouldNotRun => "no-result",
        }
    }

    /// Whether this status is a passing, landing-eligible outcome.
    pub const fn is_passing(self) -> bool {
        matches!(self, Self::Passed | Self::PassedFlaky)
    }
}

/// Outcome of publishing already-final validation evidence into the scorecard.
/// This is bookkeeping about a completed validation, not the validation verdict.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ScorecardWriteback {
    Completed,
    /// The write-back was skipped in this process because the launcher owns
    /// publication. It claims nothing about whether publication happened, and
    /// it never changes the command exit. Schema 6 and later only.
    Delegated,
    Failed {
        error: String,
    },
}

impl ScorecardWriteback {
    pub const ALL: [&'static str; 3] = ["completed", "delegated", "failed"];

    pub fn field_names(status: &str) -> &'static [&'static str] {
        match status {
            "completed" | "delegated" => &["status"],
            "failed" => &["status", "error"],
            _ => &[],
        }
    }
}

/// One node in a composite verdict (owner rule, 2026-11-01): red in the
/// base full run, re-executed in the single targeted rerun.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionNodeV1 {
    pub node: String,
    pub base_attempts: Option<u64>,
    pub base_failure_class: Option<String>,
    pub rerun_attempts: Option<u64>,
    pub rerun_failure_class: Option<String>,
    pub rerun_outcome: String,
}

/// A composite verdict record: a full run at one commit plus exactly one
/// targeted rerun of its red nodes composed into GREEN_FLAKY (or FAILED
/// when the rerun stays red). The base run's own record is unchanged;
/// this record references it, and the original failures stay counted in
/// the flaky metrics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompositionV1 {
    pub schema: String,
    pub verdict: String,
    pub commit: String,
    pub profile: String,
    pub image_digest: Option<String>,
    pub base_run_id: String,
    pub rerun_run_id: String,
    pub outer_reruns_used: u64,
    pub nodes_rerun: Vec<CompositionNodeV1>,
    pub original_failures_kept_in_flaky_metrics: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationServiceResult {
    pub schema_version: u64,
    pub commit: String,
    pub profile: String,
    /// The plan selection that was actually executed. Historical schemas did
    /// not carry this, so retained rows decode as unknown rather than being
    /// mistaken for a complete full-profile run.
    #[serde(default)]
    pub selection_mode: Option<String>,
    pub final_validate_status: FinalValidateStatus,
    /// Ordered terminal detail rendered by validation. A current producer
    /// writes the field even when no cause exists, so null is honest absence
    /// rather than a missing field.
    #[serde(default)]
    pub detail: Option<Vec<String>>,
    /// The command exit, not the validation verdict's exit. A PASSED validation
    /// may therefore carry 75 when its required scorecard write-back failed.
    pub exit_code: i32,
    pub executed_nodes: u64,
    pub executed_tests: Option<i64>,
    /// Exact terminal passes recorded by the test frameworks. Historical
    /// schemas omitted this field, so absence remains unknown rather than being
    /// reconstructed from the verdict or executed-test count.
    #[serde(default)]
    pub passed_tests: Option<i64>,
    /// Executed tests counted apart as producer-declared non-blocking
    /// diagnostics. Schema 7 and later only; absence on a schema-7 result
    /// with an executed count is malformed, not zero.
    #[serde(default)]
    pub diagnostic_failed_tests: Option<i64>,
    /// Kept separate from `final_validate_status`: a write-back failure must
    /// make the command fail loudly without rewriting a real tree verdict.
    #[serde(default)]
    pub scorecard_writeback: Option<ScorecardWriteback>,
    /// Composite-verdict record when this receipt is the targeted rerun
    /// half of a composition, or the composed receipt itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition: Option<CompositionV1>,
}

impl ValidationServiceResult {
    pub fn validated(self) -> Result<Self, String> {
        self.validate()?;
        Ok(self)
    }

    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, String> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("validation-service-result-shape: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "validation-service-result-shape: expected an object".to_string())?;
        let schema_version = object
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                "validation-service-result-schema_version: must be an unsigned integer".to_string()
            })?;
        let expected_fields: BTreeSet<&str> = match schema_version {
            HISTORICAL_SCHEMA_VERSION => HISTORICAL_FIELD_NAMES.into_iter().collect(),
            WRITEBACK_SCHEMA_VERSION => WRITEBACK_FIELD_NAMES.into_iter().collect(),
            SELECTION_SCHEMA_VERSION => SELECTION_FIELD_NAMES.into_iter().collect(),
            TEST_COUNTS_SCHEMA_VERSION => TEST_COUNTS_FIELD_NAMES.into_iter().collect(),
            DETAIL_SCHEMA_VERSION => FIELD_NAMES.into_iter().collect(),
            DIAGNOSTIC_COUNTS_SCHEMA_VERSION => DIAGNOSTIC_COUNTS_FIELD_NAMES.into_iter().collect(),
            other => {
                return Err(format!(
                    "validation-service-result-schema_version: expected {HISTORICAL_SCHEMA_VERSION}, {WRITEBACK_SCHEMA_VERSION}, {SELECTION_SCHEMA_VERSION}, {TEST_COUNTS_SCHEMA_VERSION}, {DETAIL_SCHEMA_VERSION}, {DIAGNOSTIC_COUNTS_SCHEMA_VERSION}, or {SCHEMA_VERSION}, got {other}"
                ));
            }
        };
        let actual_fields: BTreeSet<&str> = object.keys().map(String::as_str).collect();
        // Schema 6 gained one optional field after first publication:
        // `composition` (composite-verdict record). A schema-6 receipt
        // either lacks it or carries exactly it in addition.
        let with_composition: BTreeSet<&str> = expected_fields
            .iter()
            .copied()
            .chain(["composition"])
            .collect();
        if actual_fields != expected_fields && actual_fields != with_composition {
            return Err(format!(
                "validation-service-result-fields: schema {schema_version} expected {expected_fields:?}, got {actual_fields:?}"
            ));
        }
        // serde does not apply `deny_unknown_fields` to the fieldless tagged
        // variants, so hold each write-back object to its exact declared fields.
        if let Some(writeback) = object.get("scorecard_writeback").and_then(Value::as_object) {
            let status = writeback
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
            let declared: BTreeSet<&str> = ScorecardWriteback::field_names(status)
                .iter()
                .copied()
                .collect();
            let actual: BTreeSet<&str> = writeback.keys().map(String::as_str).collect();
            if !declared.is_empty() && actual != declared {
                return Err(format!(
                    "validation-service-result-shape: scorecard_writeback {status:?} expected {declared:?}, got {actual:?}"
                ));
            }
        }
        let result: Self = serde_json::from_value(value)
            .map_err(|error| format!("validation-service-result-shape: {error}"))?;
        result.validate()?;
        Ok(result)
    }

    pub fn validate(&self) -> Result<(), String> {
        if ![
            HISTORICAL_SCHEMA_VERSION,
            WRITEBACK_SCHEMA_VERSION,
            SELECTION_SCHEMA_VERSION,
            TEST_COUNTS_SCHEMA_VERSION,
            DETAIL_SCHEMA_VERSION,
            SCHEMA_VERSION,
        ]
        .contains(&self.schema_version)
        {
            return Err(format!(
                "validation-service-result-schema_version: expected {HISTORICAL_SCHEMA_VERSION}, {WRITEBACK_SCHEMA_VERSION}, {SELECTION_SCHEMA_VERSION}, {TEST_COUNTS_SCHEMA_VERSION}, {DETAIL_SCHEMA_VERSION}, or {SCHEMA_VERSION}, got {}",
                self.schema_version
            ));
        }
        if self.commit.len() != 40
            || !self
                .commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(
                "validation-service-result-commit: must be lowercase forty-hex".to_string(),
            );
        }
        if self.profile.is_empty() {
            return Err("validation-service-result-profile: must be nonempty".to_string());
        }
        if self
            .selection_mode
            .as_ref()
            .is_some_and(|selection| selection.trim().is_empty())
        {
            return Err(
                "validation-service-result-selection_mode: must be nonempty or null".to_string(),
            );
        }
        if let Some(detail) = &self.detail {
            if detail.is_empty() || detail.iter().any(|line| line.trim().is_empty()) {
                return Err(
                    "validation-service-result-detail: must be a nonempty list of nonempty strings or null"
                        .to_string(),
                );
            }
        }
        if self.schema_version < DETAIL_SCHEMA_VERSION && self.detail.is_some() {
            return Err(format!(
                "validation-service-result-historical-fields: schema {} cannot carry detail",
                self.schema_version
            ));
        }
        if self.final_validate_status != FinalValidateStatus::CouldNotRun && self.detail.is_some() {
            return Err(format!(
                "validation-service-result-detail: {} must carry null",
                self.final_validate_status.as_str()
            ));
        }
        let validation_exit = self.final_validate_status.exit_code();
        if self.schema_version == HISTORICAL_SCHEMA_VERSION {
            if self.scorecard_writeback.is_some() || self.selection_mode.is_some() {
                return Err(
                    "validation-service-result-historical-fields: schema 1 cannot carry selection_mode or scorecard_writeback"
                        .to_string(),
                );
            }
            if self.exit_code != validation_exit {
                return Err(format!(
                    "validation-service-result-exit_code: historical schema 1 {} requires {validation_exit}, got {}",
                    self.final_validate_status.as_str(),
                    self.exit_code
                ));
            }
        } else {
            if self.schema_version == WRITEBACK_SCHEMA_VERSION && self.selection_mode.is_some() {
                return Err(
                    "validation-service-result-selection_mode: schema 2 cannot carry this field"
                        .to_string(),
                );
            }
            if self.schema_version < SCHEMA_VERSION
                && self.scorecard_writeback == Some(ScorecardWriteback::Delegated)
            {
                return Err(format!(
                    "validation-service-result-scorecard_writeback: schema {} cannot carry delegated",
                    self.schema_version
                ));
            }
            if let Some(ScorecardWriteback::Failed { error }) = &self.scorecard_writeback {
                if error.trim().is_empty() {
                    return Err(
                        "validation-service-result-scorecard_writeback: failed requires a nonempty error"
                            .to_string(),
                    );
                }
            }
            // A write-back failure is reported in the result; it does not
            // change the command exit. Producers before 2026-10-08 turned a
            // PASSED run's failed write-back into exit 75, so records they
            // wrote stay readable.
            let expected_command_exit = validation_exit;
            let historical_writeback_exit = self.final_validate_status.is_passing()
                && matches!(
                    self.scorecard_writeback,
                    Some(ScorecardWriteback::Failed { .. })
                )
                && self.exit_code == 75;
            if self.exit_code != expected_command_exit && !historical_writeback_exit {
                return Err(format!(
                    "validation-service-result-exit_code: {} with scorecard_writeback {:?} requires {expected_command_exit}, got {}",
                    self.final_validate_status.as_str(),
                    self.scorecard_writeback,
                    self.exit_code
                ));
            }
        }
        if self.executed_tests.is_some_and(|count| count < 0) {
            return Err(
                "validation-service-result-executed_tests: must be nonnegative or null".to_string(),
            );
        }
        if self.schema_version < TEST_COUNTS_SCHEMA_VERSION && self.passed_tests.is_some() {
            return Err(format!(
                "validation-service-result-historical-fields: schema {} cannot carry passed_tests",
                self.schema_version
            ));
        }
        if self.schema_version >= TEST_COUNTS_SCHEMA_VERSION
            && self.executed_tests.is_some()
            && self.passed_tests.is_none()
        {
            return Err(
                "validation-service-result-passed_tests: current result with executed_tests requires an exact count"
                    .to_string(),
            );
        }
        if self.passed_tests.is_some_and(|count| count < 0) {
            return Err(
                "validation-service-result-passed_tests: must be nonnegative or null".to_string(),
            );
        }
        if let Some(passed) = self.passed_tests {
            let executed = self.executed_tests.ok_or_else(|| {
                "validation-service-result-passed_tests: cannot be present when executed_tests is null"
                    .to_string()
            })?;
            if passed > executed {
                return Err(format!(
                    "validation-service-result-passed_tests: {passed} exceeds executed_tests {executed}"
                ));
            }
        }
        if self.schema_version < DIAGNOSTIC_COUNTS_SCHEMA_VERSION
            && self.diagnostic_failed_tests.is_some()
        {
            return Err(format!(
                "validation-service-result-historical-fields: schema {} cannot carry diagnostic_failed_tests",
                self.schema_version
            ));
        }
        if self.schema_version >= DIAGNOSTIC_COUNTS_SCHEMA_VERSION
            && self.executed_tests.is_some()
            && self.diagnostic_failed_tests.is_none()
        {
            return Err(
                "validation-service-result-diagnostic_failed_tests: current result with executed_tests requires an exact count"
                    .to_string(),
            );
        }
        if self.diagnostic_failed_tests.is_some_and(|count| count < 0) {
            return Err(
                "validation-service-result-diagnostic_failed_tests: must be nonnegative or null"
                    .to_string(),
            );
        }
        if let Some(diagnostic) = self.diagnostic_failed_tests {
            let executed = self.executed_tests.ok_or_else(|| {
                "validation-service-result-diagnostic_failed_tests: cannot be present when executed_tests is null"
                    .to_string()
            })?;
            if diagnostic > executed {
                return Err(format!(
                    "validation-service-result-diagnostic_failed_tests: {diagnostic} exceeds executed_tests {executed}"
                ));
            }
        }
        // An unknown count does not stop a PASSED result from being reported:
        // whether a run's counts qualify it as landing evidence is ci-hub's
        // qualifying-receipt predicate's decision (profile full, at least one
        // executed test, passed == executed). A count that IS present must
        // still agree with itself. From schema 7 a passing result accounts
        // for every executed test explicitly: each one either passed or is a
        // producer-declared diagnostic. Diagnostics are never passes, and a
        // failure that is not a declared diagnostic leaves the sum short.
        if let (true, Some(passed), Some(executed)) = (
            self.schema_version >= TEST_COUNTS_SCHEMA_VERSION
                && self.final_validate_status.is_passing(),
            self.passed_tests,
            self.executed_tests,
        ) {
            match self.diagnostic_failed_tests {
                Some(diagnostic) => {
                    if passed.checked_add(diagnostic) != Some(executed) {
                        return Err(format!(
                            "validation-service-result-passed_tests: current PASSED result requires passed_tests + diagnostic_failed_tests == executed_tests, got {passed} + {diagnostic} != {executed}"
                        ));
                    }
                }
                None => {
                    if passed != executed {
                        return Err(format!(
                            "validation-service-result-passed_tests: current PASSED result requires passed_tests == executed_tests, got {passed} != {executed}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> ValidationServiceResult {
        ValidationServiceResult {
            schema_version: SCHEMA_VERSION,
            commit: "a".repeat(40),
            profile: "full".into(),
            selection_mode: Some("full".into()),
            final_validate_status: FinalValidateStatus::Passed,
            detail: None,
            exit_code: 0,
            executed_nodes: 76,
            executed_tests: Some(2129),
            passed_tests: Some(2129),
            diagnostic_failed_tests: Some(0),
            scorecard_writeback: None,
            composition: None,
        }
        .validated()
        .unwrap()
    }

    #[test]
    fn passing_result_accounts_for_diagnostics_explicitly() {
        // 42 executed = 35 passed + 7 producer-declared diagnostics.
        let mut result = valid();
        result.executed_tests = Some(42);
        result.passed_tests = Some(35);
        result.diagnostic_failed_tests = Some(7);
        result.validate().unwrap();

        // A diagnostic counted as a pass leaves the sum over-executed.
        result.passed_tests = Some(42);
        assert!(
            result
                .validate()
                .unwrap_err()
                .contains("passed_tests + diagnostic_failed_tests == executed_tests")
        );

        // An unlisted failure leaves the sum short.
        result.passed_tests = Some(34);
        assert!(
            result
                .validate()
                .unwrap_err()
                .contains("passed_tests + diagnostic_failed_tests == executed_tests")
        );

        // Diagnostics are never silently dropped on a current result.
        result.passed_tests = Some(35);
        result.diagnostic_failed_tests = None;
        assert!(result.validate().unwrap_err().contains(
            "diagnostic_failed_tests: current result with executed_tests requires an exact count"
        ));

        // Schemas before 7 cannot carry the field at all.
        result.schema_version = DETAIL_SCHEMA_VERSION;
        result.diagnostic_failed_tests = Some(7);
        assert!(
            result
                .validate()
                .unwrap_err()
                .contains("cannot carry diagnostic_failed_tests")
        );
    }

    #[test]
    fn current_shape_round_trips_and_refuses_unknown_fields() {
        let encoded = serde_json::to_vec(&valid()).unwrap();
        assert_eq!(
            ValidationServiceResult::from_json_slice(&encoded).unwrap(),
            valid()
        );
        let mut value = serde_json::to_value(valid()).unwrap();
        value["future"] = Value::Bool(true);
        let error = ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
            .unwrap_err();
        assert!(error.contains("validation-service-result-fields"));
        assert!(error.contains("future"));
    }

    #[test]
    fn status_and_exit_must_agree() {
        let mut result = valid();
        result.exit_code = 1;
        assert_eq!(
            result.validate().unwrap_err(),
            "validation-service-result-exit_code: PASSED with scorecard_writeback None requires 0, got 1"
        );
    }

    #[test]
    fn failed_writeback_preserves_pass_and_its_exit() {
        let mut result = valid();
        result.scorecard_writeback = Some(ScorecardWriteback::Failed {
            error: "fixture refusal".into(),
        });
        result.validate().unwrap();

        // Records written before the write-back stopped changing the exit.
        result.exit_code = 75;
        result.validate().unwrap();

        result.exit_code = 1;
        assert!(result.validate().unwrap_err().contains("requires 0, got 1"));
    }

    #[test]
    fn genuine_could_not_run_remains_distinct_from_writeback_failure() {
        for detail in [
            vec![
                "refused by: pre.reverie_pin".into(),
                "recorded pin is not an ancestor".into(),
            ],
            vec![
                "REFUSED ON COMPLETENESS: ZERO tests executed".into(),
                "0 test(s) executed, 0 passed, 4 filtered".into(),
            ],
        ] {
            let result = ValidationServiceResult {
                schema_version: SCHEMA_VERSION,
                commit: "a".repeat(40),
                profile: "full".into(),
                selection_mode: Some("full".into()),
                final_validate_status: FinalValidateStatus::CouldNotRun,
                detail: Some(detail.clone()),
                exit_code: 75,
                executed_nodes: 0,
                executed_tests: None,
                passed_tests: None,
                diagnostic_failed_tests: None,
                scorecard_writeback: None,
                composition: None,
            }
            .validated()
            .unwrap();
            assert_eq!(
                result.final_validate_status,
                FinalValidateStatus::CouldNotRun
            );
            assert_eq!(result.detail, Some(detail));
            assert_eq!(result.scorecard_writeback, None);
        }
    }

    #[test]
    fn historical_schemas_are_readable_only_in_their_exact_old_shapes() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(HISTORICAL_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("selection_mode");
        value.as_object_mut().unwrap().remove("detail");
        value.as_object_mut().unwrap().remove("passed_tests");
        value
            .as_object_mut()
            .unwrap()
            .remove("diagnostic_failed_tests");
        value.as_object_mut().unwrap().remove("scorecard_writeback");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, HISTORICAL_SCHEMA_VERSION);
        assert_eq!(decoded.selection_mode, None);
        assert_eq!(decoded.passed_tests, None);
        assert_eq!(decoded.scorecard_writeback, None);

        value["scorecard_writeback"] = Value::Null;
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 1 expected")
        );

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(WRITEBACK_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("selection_mode");
        value.as_object_mut().unwrap().remove("detail");
        value.as_object_mut().unwrap().remove("passed_tests");
        value
            .as_object_mut()
            .unwrap()
            .remove("diagnostic_failed_tests");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, WRITEBACK_SCHEMA_VERSION);
        assert_eq!(decoded.selection_mode, None);
        assert_eq!(decoded.passed_tests, None);

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(SELECTION_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("detail");
        value.as_object_mut().unwrap().remove("passed_tests");
        value
            .as_object_mut()
            .unwrap()
            .remove("diagnostic_failed_tests");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, SELECTION_SCHEMA_VERSION);
        assert_eq!(decoded.passed_tests, None);

        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(TEST_COUNTS_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("detail");
        value
            .as_object_mut()
            .unwrap()
            .remove("diagnostic_failed_tests");
        let decoded =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.schema_version, TEST_COUNTS_SCHEMA_VERSION);
        assert_eq!(decoded.passed_tests, Some(2129));
        assert_eq!(decoded.detail, None);

        // Schema 5 had the current field set and the two-variant write-back.
        for writeback in [
            Value::Null,
            serde_json::json!({"status": "completed"}),
            serde_json::json!({"status": "failed", "error": "fixture refusal"}),
        ] {
            let mut value = serde_json::to_value(valid()).unwrap();
            value["schema_version"] = Value::from(DETAIL_SCHEMA_VERSION);
            value
                .as_object_mut()
                .unwrap()
                .remove("diagnostic_failed_tests");
            if writeback["status"] == "failed" {
                value["exit_code"] = Value::from(75);
            }
            value["scorecard_writeback"] = writeback.clone();
            let decoded =
                ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                    .unwrap();
            assert_eq!(decoded.schema_version, DETAIL_SCHEMA_VERSION);
            assert_eq!(
                serde_json::to_value(&decoded.scorecard_writeback).unwrap(),
                writeback
            );
        }
        let mut value = serde_json::to_value(valid()).unwrap();
        value["schema_version"] = Value::from(DETAIL_SCHEMA_VERSION);
        value.as_object_mut().unwrap().remove("detail");
        value
            .as_object_mut()
            .unwrap()
            .remove("diagnostic_failed_tests");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 5 expected")
        );
    }

    #[test]
    fn delegated_writeback_is_schema_6_only_and_never_changes_the_exit() {
        let mut delegated = valid();
        delegated.scorecard_writeback = Some(ScorecardWriteback::Delegated);
        delegated.validate().unwrap();
        let encoded = serde_json::to_value(&delegated).unwrap();
        assert_eq!(
            encoded["scorecard_writeback"],
            serde_json::json!({"status": "delegated"})
        );
        assert_eq!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&encoded).unwrap())
                .unwrap(),
            delegated
        );

        // Delegation is not a write-back failure: PASSED keeps exit 0.
        let mut loud = delegated.clone();
        loud.exit_code = 75;
        assert!(
            loud.validate().unwrap_err().contains("requires 0, got 75"),
            "delegated PASSED must not demand the write-back-failure exit"
        );
        for status in [
            FinalValidateStatus::Failed,
            FinalValidateStatus::CouldNotRun,
        ] {
            let mut other = delegated.clone();
            other.final_validate_status = status;
            other.exit_code = status.exit_code();
            if status == FinalValidateStatus::CouldNotRun {
                other.executed_tests = None;
                other.passed_tests = None;
                other.diagnostic_failed_tests = None;
            } else {
                other.passed_tests = Some(2128);
            }
            other.validate().unwrap();
        }

        for version in [
            WRITEBACK_SCHEMA_VERSION,
            SELECTION_SCHEMA_VERSION,
            TEST_COUNTS_SCHEMA_VERSION,
            DETAIL_SCHEMA_VERSION,
        ] {
            let mut historical = delegated.clone();
            historical.schema_version = version;
            if version == WRITEBACK_SCHEMA_VERSION {
                historical.selection_mode = None;
            }
            if version < TEST_COUNTS_SCHEMA_VERSION {
                historical.passed_tests = None;
            }
            assert_eq!(
                historical.validate().unwrap_err(),
                format!(
                    "validation-service-result-scorecard_writeback: schema {version} cannot carry delegated"
                )
            );
        }
        let mut value = encoded.clone();
        value["schema_version"] = Value::from(DETAIL_SCHEMA_VERSION);
        value
            .as_object_mut()
            .unwrap()
            .remove("diagnostic_failed_tests");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 5 cannot carry delegated")
        );

        for status in ["delegated", "completed"] {
            let mut extra = encoded.clone();
            extra["scorecard_writeback"] = serde_json::json!({"status": status, "error": "x"});
            assert!(
                ValidationServiceResult::from_json_slice(&serde_json::to_vec(&extra).unwrap())
                    .unwrap_err()
                    .contains("validation-service-result-shape"),
                "{status} with an extra field must be refused"
            );
        }
    }

    #[test]
    fn current_schema_refuses_a_missing_writeback_field() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("scorecard_writeback");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 7 expected")
        );
    }

    #[test]
    fn current_schema_refuses_a_missing_selection_field() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("selection_mode");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 7 expected")
        );
    }

    #[test]
    fn current_schema_refuses_a_missing_passed_count() {
        let mut value = serde_json::to_value(valid()).unwrap();
        value.as_object_mut().unwrap().remove("passed_tests");
        assert!(
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("schema 7 expected")
        );
    }

    #[test]
    fn current_detail_is_required_nullable_and_only_names_no_result() {
        let mut missing = serde_json::to_value(valid()).unwrap();
        missing.as_object_mut().unwrap().remove("detail");
        let error =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&missing).unwrap())
                .unwrap_err();
        assert!(error.contains("schema 7 expected"), "{error}");

        for status in [FinalValidateStatus::Passed, FinalValidateStatus::Failed] {
            let mut result = valid();
            result.final_validate_status = status;
            result.exit_code = status.exit_code();
            result.detail = Some(vec!["must not survive".into()]);
            let error = result.validate().unwrap_err();
            assert!(
                error.contains(&format!("{} must carry null", status.as_str())),
                "{error}"
            );
        }

        let mut no_result = valid();
        no_result.final_validate_status = FinalValidateStatus::CouldNotRun;
        no_result.exit_code = 75;
        no_result.executed_tests = None;
        no_result.passed_tests = None;
        no_result.diagnostic_failed_tests = None;
        no_result.detail = None;
        no_result.validate().expect("genuine absence stays null");
        no_result.detail = Some(vec![]);
        assert!(no_result.validate().unwrap_err().contains("nonempty list"));
        no_result.detail = Some(vec![" ".into()]);
        assert!(no_result.validate().unwrap_err().contains("nonempty list"));

        let mut malformed = serde_json::to_value(&no_result).unwrap();
        malformed["detail"] = Value::String("not an ordered list".into());
        let error =
            ValidationServiceResult::from_json_slice(&serde_json::to_vec(&malformed).unwrap())
                .unwrap_err();
        assert!(error.contains("validation-service-result-shape"), "{error}");
    }

    #[test]
    fn composition_record_round_trips_and_stays_explicit() {
        let mut result = valid();
        result.final_validate_status = FinalValidateStatus::Failed;
        result.exit_code = 1;
        result.composition = Some(CompositionV1 {
            schema: "composite-verdict/v1".into(),
            verdict: "GREEN_FLAKY".into(),
            commit: result.commit.clone(),
            profile: "full".into(),
            image_digest: Some("sha256:deadbeef".into()),
            base_run_id: "validate-base-1".into(),
            rerun_run_id: "validate-rerun-1".into(),
            outer_reruns_used: 1,
            nodes_rerun: vec![CompositionNodeV1 {
                node: "e2e.manifest_compat".into(),
                base_attempts: Some(2),
                base_failure_class: Some("understood_infrastructure_failure".into()),
                rerun_attempts: Some(1),
                rerun_failure_class: None,
                rerun_outcome: "PASS".into(),
            }],
            original_failures_kept_in_flaky_metrics: true,
        });
        let bytes = serde_json::to_vec(&result).unwrap();
        let back = ValidationServiceResult::from_json_slice(&bytes)
            .and_then(|r| r.validated())
            .unwrap();
        assert_eq!(back, result);
        assert_eq!(
            back.composition.unwrap().nodes_rerun[0].rerun_outcome,
            "PASS"
        );
    }

    #[test]
    fn current_pass_refuses_inexact_or_contradictory_passed_counts() {
        let mut missing = valid();
        missing.passed_tests = None;
        assert!(
            missing
                .validate()
                .unwrap_err()
                .contains("requires an exact count")
        );

        let mut lower = valid();
        lower.passed_tests = Some(2128);
        assert!(
            lower
                .validate()
                .unwrap_err()
                .contains("requires passed_tests + diagnostic_failed_tests == executed_tests")
        );

        let mut greater = valid();
        greater.passed_tests = Some(2130);
        assert!(
            greater
                .validate()
                .unwrap_err()
                .contains("exceeds executed_tests")
        );

        let mut missing_executed = valid();
        missing_executed.executed_tests = None;
        assert!(
            missing_executed
                .validate()
                .unwrap_err()
                .contains("cannot be present when executed_tests is null")
        );

        // Both counts unknown: the PASSED result is reported; ci-hub's
        // qualifying-receipt predicate, not this schema, refuses it as
        // landing evidence.
        let mut unknown = valid();
        unknown.executed_tests = None;
        unknown.passed_tests = None;
        unknown.diagnostic_failed_tests = None;
        unknown.validate().unwrap();
    }

    #[test]
    fn current_failure_and_no_result_keep_distinct_count_semantics() {
        let mut failed = valid();
        failed.final_validate_status = FinalValidateStatus::Failed;
        failed.exit_code = 1;
        failed.passed_tests = Some(2128);
        failed.validate().unwrap();

        let mut failed_missing = failed.clone();
        failed_missing.passed_tests = None;
        assert!(
            failed_missing
                .validate()
                .unwrap_err()
                .contains("current result with executed_tests requires an exact count")
        );

        let mut no_result = valid();
        no_result.final_validate_status = FinalValidateStatus::CouldNotRun;
        no_result.exit_code = 75;
        no_result.executed_tests = None;
        no_result.passed_tests = None;
        no_result.diagnostic_failed_tests = None;
        no_result.validate().unwrap();
    }

    #[test]
    fn checked_schema_projection_matches_the_shared_type() {
        let schema: Value =
            serde_json::from_str(include_str!("../validation-service-result-schema.json")).unwrap();
        let fields: Vec<&str> = schema["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(fields, DIAGNOSTIC_COUNTS_FIELD_NAMES);
        assert_eq!(schema["schema_version"], SCHEMA_VERSION);
        let outcomes = schema["outcomes"].as_object().unwrap();
        let declared: BTreeSet<&str> = outcomes.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = FinalValidateStatus::ALL
            .iter()
            .map(|status| status.as_str())
            .collect();
        assert_eq!(declared, expected);
        for status in FinalValidateStatus::ALL {
            let row = &outcomes[status.as_str()];
            assert_eq!(row["validation_exit_code"], status.exit_code());
            assert_eq!(row["state"], status.service_state());
            assert_eq!(row["result"], status.service_result());
        }
        let writeback = schema["scorecard_writeback"].as_object().unwrap();
        assert_eq!(writeback["nullable"], true);
        let variants = writeback["variants"].as_object().unwrap();
        let declared: BTreeSet<&str> = variants.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = ScorecardWriteback::ALL.into_iter().collect();
        assert_eq!(declared, expected);
        for status in ScorecardWriteback::ALL {
            let fields: Vec<&str> = variants[status]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(fields, ScorecardWriteback::field_names(status));
        }
    }
}
