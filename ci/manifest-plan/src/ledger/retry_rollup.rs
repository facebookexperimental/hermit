//! One retry-count schema from innermost test to run ledger (owner policy,
//! 2026-10-30): every failing test gets a bounded retry at the innermost
//! harness; classification labels attempts but does not gate the retry.
//!
//! This module only defines and derives the record. It changes no retry
//! behaviour: producers emit [`RetryRecordV1`], and the ledger carries
//! [`RetryRollupV1`] beside `framework_retries_v1`.

use super::*;
use crate::runner::MAX_ATTEMPTS_PER_CELL;

/// Per-test retry record emitted by every innermost harness (e2e
/// test-harness, nextest wrapper, Buck ingest) in the same shape.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRecordV1 {
    pub test: String,
    pub mode: String,
    pub backend: Option<String>,
    pub attempts: u64,
    pub retries: u64,
    pub attempt_outcomes: Vec<String>,
    /// Diagnosis class per attempt (e.g. `product_failure`, `pass`);
    /// never used to decide whether a retry happened.
    pub attempt_classes: Vec<Option<String>>,
    /// Error kind per attempt (e.g. `infrastructure` for an
    /// understood-infrastructure signature, `timeout` for no_result);
    /// the signature half of the diagnosis, recorded beside the class so
    /// a later flakiness investigation can judge the infra classifier.
    #[serde(default)]
    pub attempt_error_kinds: Vec<Option<String>>,
    /// Named cause per attempt (`skid_overshoot`, `host_input_changed`,
    /// or the error kind), so skid-overshoot keeps its own bucket,
    /// separate from other understood-infrastructure causes.
    #[serde(default)]
    pub attempt_signatures: Vec<Option<String>>,
    pub final_outcome: String,
    pub flaky_pass: bool,
}

/// Run-level rollup recorded in the ledger.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryRollupV1 {
    pub schema: u32,
    /// The one e2e bound, from `MAX_ATTEMPTS_PER_CELL`. (The nextest skid
    /// helper's separate bound of 5 attempts predates this schema and is
    /// recorded per record, not by this field.)
    pub max_attempts_per_test: u64,
    pub total_retries: u64,
    pub retried_tests: u64,
    pub flaky_passes: u64,
    /// Retries (failed attempts beyond none; one per failed attempt) by
    /// that attempt's failure class, for classifier-trust reporting.
    /// Diagnosis only: it does not change the landing signal.
    #[serde(default)]
    pub retries_by_class: std::collections::BTreeMap<String, u64>,
    /// Retries grouped two levels: by failure class (the four classes,
    /// unchanged), in `retries_by_class`, and within a class by
    /// subcategory (`retries_by_subcategory`, class -> subcategory ->
    /// count). Understood-infrastructure subcategories are the stable
    /// typed signatures: `pmu_skid`, `host_input_changed`,
    /// `sandbox_denied`.
    #[serde(default)]
    pub retries_by_subcategory:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>,
    /// Flaky passes whose every failed attempt was
    /// `understood_infrastructure_failure`. Reported separately so the
    /// owner can later decide whether these count as clean; today they
    /// still make the run green-flaky like any other retry.
    #[serde(default)]
    pub infra_recovered: u64,
    /// Zero retries run-wide is the clean score.
    pub clean: bool,
    pub tests: Vec<RetryRecordV1>,
}

impl RetryRollupV1 {
    pub fn from_records(tests: Vec<RetryRecordV1>) -> Self {
        let total_retries = tests.iter().map(|t| t.retries).sum();
        let retried_tests = tests.iter().filter(|t| t.retries > 0).count() as u64;
        let flaky_passes = tests.iter().filter(|t| t.flaky_pass).count() as u64;
        let mut retries_by_class: std::collections::BTreeMap<String, u64> = Default::default();
        let mut retries_by_subcategory: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, u64>,
        > = Default::default();
        let mut infra_recovered = 0u64;
        for test in &tests {
            for (i, (class, outcome)) in test
                .attempt_classes
                .iter()
                .zip(&test.attempt_outcomes)
                .enumerate()
            {
                if outcome != "PASS" {
                    if let Some(class) = class {
                        *retries_by_class.entry(class.clone()).or_default() += 1;
                    }
                    let signature = test.attempt_signatures.get(i).and_then(|s| s.as_deref());
                    // The four classes stay as they are; skid is a
                    // subcategory of understood infrastructure, not a
                    // fifth class. Nextest skid records carry the legacy
                    // class label `skid_overshoot`; normalize it here.
                    let class = match class.as_deref() {
                        Some("skid_overshoot") => {
                            Some("understood_infrastructure_failure".to_string())
                        }
                        other => other.map(str::to_string),
                    };
                    let subcategory = match signature {
                        Some("skid_overshoot") => "pmu_skid",
                        Some("host_input_changed") => "host_input_changed",
                        Some("sandbox-denied") | Some("sandbox_denied") => "sandbox_denied",
                        Some(other) => other,
                        None => "unspecified",
                    }
                    .to_string();
                    let class_key = class.unwrap_or_else(|| "unclassified".to_string());
                    *retries_by_subcategory
                        .entry(class_key)
                        .or_default()
                        .entry(subcategory)
                        .or_default() += 1;
                }
            }
            let failed_classes: Vec<&String> = test
                .attempt_classes
                .iter()
                .zip(&test.attempt_outcomes)
                .filter(|(_, outcome)| *outcome != "PASS")
                .filter_map(|(class, _)| class.as_ref())
                .collect();
            if test.flaky_pass
                && !failed_classes.is_empty()
                && failed_classes
                    .iter()
                    .all(|c| c.as_str() == "understood_infrastructure_failure")
            {
                infra_recovered += 1;
            }
        }
        Self {
            schema: 1,
            max_attempts_per_test: MAX_ATTEMPTS_PER_CELL,
            total_retries,
            retried_tests,
            flaky_passes,
            retries_by_class,
            retries_by_subcategory,
            infra_recovered,
            clean: total_retries == 0,
            tests,
        }
    }

    /// Derive the rollup from the existing framework-retries census.
    /// Attempts beyond the first are retries; a retried test whose final
    /// outcome is PASS is a flaky pass.
    pub fn from_framework_retries(retries: &FrameworkRetriesV1) -> Self {
        let tests = retries
            .cells
            .iter()
            .map(|cell| RetryRecordV1 {
                test: cell.cell.test.clone(),
                mode: cell.cell.mode.clone(),
                backend: Some(cell.cell.backend.clone()),
                attempts: cell.attempts,
                retries: cell.attempts.saturating_sub(1),
                // The census retains first and final outcomes; with the
                // harness bound of 2 attempts that is the whole history.
                attempt_outcomes: if cell.attempts > 1 {
                    vec![cell.first_outcome.clone(), cell.final_outcome.clone()]
                } else {
                    vec![cell.first_outcome.clone()]
                },
                attempt_classes: if cell.attempts > 1 {
                    vec![
                        cell.first_failure_class.clone(),
                        if cell.final_outcome == "PASS" {
                            None
                        } else {
                            cell.first_failure_class.clone()
                        },
                    ]
                } else {
                    vec![cell.first_failure_class.clone()]
                },
                attempt_signatures: if cell.attempts > 1 {
                    vec![
                        cell.first_signature.clone(),
                        if cell.final_outcome == "PASS" {
                            None
                        } else {
                            cell.first_signature.clone()
                        },
                    ]
                } else {
                    vec![cell.first_signature.clone()]
                },
                attempt_error_kinds: if cell.attempts > 1 {
                    vec![
                        cell.first_error_kind.clone(),
                        if cell.final_outcome == "PASS" {
                            None
                        } else {
                            cell.first_error_kind.clone()
                        },
                    ]
                } else {
                    vec![cell.first_error_kind.clone()]
                },
                final_outcome: cell.final_outcome.clone(),
                flaky_pass: cell.final_outcome == "PASS",
            })
            .collect();
        Self::from_records(tests)
    }

    pub fn from_inputs(inputs: &BTreeMap<String, Vec<u8>>) -> Result<Self, String> {
        Ok(Self::from_framework_retries(
            &FrameworkRetriesV1::from_inputs(inputs)?,
        ))
    }
}

/// The landing signal for a validate run (Step 4): red on any product
/// failure or an incomplete run; otherwise green-clean when no retry ran,
/// green-flaky (still landing-eligible) with the top flaky tests named.
pub fn landing_signal(
    product_failures: usize,
    validation_complete: bool,
    run_succeeded: bool,
    rollup: &RetryRollupV1,
    extra_retries: u64,
    extra_flaky_tests: Vec<String>,
) -> (&'static str, Vec<String>) {
    if product_failures > 0 || !validation_complete || !run_succeeded {
        return ("red", Vec::new());
    }
    if rollup.total_retries == 0 && extra_retries == 0 {
        return ("green-clean", Vec::new());
    }
    let mut flaky: Vec<&RetryRecordV1> = rollup
        .tests
        .iter()
        .filter(|t| t.flaky_pass && t.retries > 0)
        .collect();
    flaky.sort_by(|a, b| b.retries.cmp(&a.retries).then_with(|| a.test.cmp(&b.test)));
    let mut top: Vec<String> = flaky
        .iter()
        .map(|t| format!("{} ({} retries)", t.test, t.retries))
        .collect();
    top.extend(extra_flaky_tests);
    // Global top by retries, parsed back out of the "(N retries)" suffix.
    top.sort_by(|a, b| {
        let n = |s: &str| {
            s.rsplit('(')
                .next()
                .and_then(|r| r.split(' ').next())
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
        };
        n(b).cmp(&n(a)).then_with(|| a.cmp(b))
    });
    top.truncate(10);
    ("green-flaky", top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollup_counts_retries_and_clean() {
        let clean = RetryRollupV1::from_records(vec![]);
        assert!(clean.clean && clean.total_retries == 0);
        assert_eq!(clean.max_attempts_per_test, MAX_ATTEMPTS_PER_CELL);
        let r = RetryRecordV1 {
            test: "fixture/x".into(),
            mode: "verify".into(),
            backend: Some("dbt".into()),
            attempts: 2,
            retries: 1,
            attempt_outcomes: vec!["FAIL".into(), "PASS".into()],
            attempt_classes: vec![Some("product_failure".into()), None],
            attempt_error_kinds: vec![None, None],
            attempt_signatures: vec![None, None],
            final_outcome: "PASS".into(),
            flaky_pass: true,
        };
        let rollup = RetryRollupV1::from_records(vec![r]);
        assert!(!rollup.clean);
        assert_eq!(
            (
                rollup.total_retries,
                rollup.retried_tests,
                rollup.flaky_passes
            ),
            (1, 1, 1)
        );
        let value = serde_json::to_value(&rollup).unwrap();
        assert_eq!(
            serde_json::from_value::<RetryRollupV1>(value).unwrap(),
            rollup
        );
    }

    #[test]
    fn rollup_reports_retries_by_class_and_infra_recovered_without_changing_signal() {
        let infra = RetryRecordV1 {
            test: "fixture/infra".into(),
            mode: "verify".into(),
            backend: Some("ptrace".into()),
            attempts: 2,
            retries: 1,
            attempt_outcomes: vec!["ERROR".into(), "PASS".into()],
            attempt_classes: vec![Some("understood_infrastructure_failure".into()), None],
            attempt_error_kinds: vec![Some("infrastructure".into()), None],
            attempt_signatures: vec![Some("host_input_changed".into()), None],
            final_outcome: "PASS".into(),
            flaky_pass: true,
        };
        let product = RetryRecordV1 {
            test: "fixture/product".into(),
            mode: "verify".into(),
            backend: None,
            attempts: 2,
            retries: 1,
            attempt_outcomes: vec!["FAIL".into(), "PASS".into()],
            attempt_classes: vec![Some("product_failure".into()), None],
            attempt_error_kinds: vec![None, None],
            attempt_signatures: vec![None, None],
            final_outcome: "PASS".into(),
            flaky_pass: true,
        };
        let rollup = RetryRollupV1::from_records(vec![infra, product]);
        assert_eq!(rollup.infra_recovered, 1);
        assert_eq!(
            rollup
                .retries_by_class
                .get("understood_infrastructure_failure"),
            Some(&1)
        );
        assert_eq!(rollup.retries_by_class.get("product_failure"), Some(&1));
        // Diagnosis only: an infra-only recovered run is still green-flaky.
        let (signal, _) = landing_signal(0, true, true, &rollup, 0, vec![]);
        assert_eq!(signal, "green-flaky");
        // Round-trip, and a pre-existing record without the new fields parses.
        let value = serde_json::to_value(&rollup).unwrap();
        assert_eq!(
            serde_json::from_value::<RetryRollupV1>(value.clone()).unwrap(),
            rollup
        );
        let legacy = r#"{"test":"fixture/old","mode":"verify","backend":null,"attempts":1,"retries":0,"attempt_outcomes":["PASS"],"attempt_classes":[null],"final_outcome":"PASS","flaky_pass":false}"#;
        assert!(serde_json::from_str::<RetryRecordV1>(legacy).is_ok());
    }

    #[test]
    fn landing_signal_distinguishes_clean_flaky_and_red() {
        let clean = RetryRollupV1::from_records(vec![]);
        assert_eq!(
            landing_signal(0, true, true, &clean, 0, vec![]),
            ("green-clean", vec![])
        );
        assert_eq!(landing_signal(1, true, true, &clean, 0, vec![]).0, "red");
        assert_eq!(landing_signal(0, false, true, &clean, 0, vec![]).0, "red");
        assert_eq!(landing_signal(0, true, false, &clean, 0, vec![]).0, "red");
        let flaky = RetryRollupV1::from_records(vec![RetryRecordV1 {
            test: "fixture/x".into(),
            mode: "verify".into(),
            backend: None,
            attempts: 2,
            retries: 1,
            attempt_outcomes: vec!["FAIL".into(), "PASS".into()],
            attempt_classes: vec![None, None],
            attempt_error_kinds: vec![None, None],
            attempt_signatures: vec![None, None],
            final_outcome: "PASS".into(),
            flaky_pass: true,
        }]);
        let (signal, top) = landing_signal(0, true, true, &flaky, 0, vec![]);
        assert_eq!(
            landing_signal(
                0,
                true,
                true,
                &clean,
                2,
                vec!["nextest/x (1 retries)".into()]
            )
            .0,
            "green-flaky"
        );
        assert_eq!(signal, "green-flaky");
        assert_eq!(top, ["fixture/x (1 retries)"]);
    }
}
