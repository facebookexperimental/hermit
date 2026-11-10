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
    /// The generic e2e bound, from `MAX_ATTEMPTS_PER_CELL`. A skid-only
    /// history may run to the separate skid budget
    /// (`MAX_SKID_ATTEMPTS_PER_CELL`, the shared
    /// `detcore_model::SKID_RETRY_MAX_ATTEMPTS`); such a record's own
    /// attempt count says so, not this field.
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
        // Per-attempt values come from the census's retained vectors
        // when it carries them (one entry per attempt, from the raw
        // rows). Records written before those fields retain only the
        // first attempt: for those, first+final is the whole history at
        // the generic bound of 2, and a longer legacy record's
        // intermediate attempts are marked unknown (empty outcome, no
        // class/signature/kind) rather than reconstructed from the
        // first attempt — copying the first attempt's class onto a
        // later product attempt would mislabel it as infrastructure.
        fn retained_or_legacy<T: Clone>(
            cell: &RetriedCellV1,
            retained: &[T],
            first: T,
            final_at_two: T,
            legacy_final: T,
            unknown: T,
        ) -> Vec<T> {
            if retained.len() as u64 == cell.attempts {
                return retained.to_vec();
            }
            if cell.attempts <= 1 {
                return vec![first];
            }
            if cell.attempts == 2 {
                return vec![first, final_at_two];
            }
            let mut values = vec![unknown; cell.attempts as usize];
            values[0] = first;
            *values.last_mut().expect("attempts >= 3") = legacy_final;
            values
        }
        let tests = retries
            .cells
            .iter()
            .map(|cell| RetryRecordV1 {
                test: cell.cell.test.clone(),
                mode: cell.cell.mode.clone(),
                backend: Some(cell.cell.backend.clone()),
                attempts: cell.attempts,
                retries: cell.attempts.saturating_sub(1),
                attempt_outcomes: retained_or_legacy(
                    cell,
                    &cell.attempt_outcomes,
                    cell.first_outcome.clone(),
                    cell.final_outcome.clone(),
                    cell.final_outcome.clone(),
                    String::new(),
                ),
                attempt_classes: retained_or_legacy(
                    cell,
                    &cell.attempt_failure_classes,
                    cell.first_failure_class.clone(),
                    if cell.final_outcome == "PASS" {
                        None
                    } else {
                        cell.first_failure_class.clone()
                    },
                    None,
                    None,
                ),
                attempt_signatures: retained_or_legacy(
                    cell,
                    &cell.attempt_signatures,
                    cell.first_signature.clone(),
                    if cell.final_outcome == "PASS" {
                        None
                    } else {
                        cell.first_signature.clone()
                    },
                    None,
                    None,
                ),
                attempt_error_kinds: retained_or_legacy(
                    cell,
                    &cell.attempt_error_kinds,
                    cell.first_error_kind.clone(),
                    if cell.final_outcome == "PASS" {
                        None
                    } else {
                        cell.first_error_kind.clone()
                    },
                    None,
                    None,
                ),
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
    fn rollup_from_census_keeps_each_attempts_own_class() {
        // A mixed history (skid, skid, product FAIL) must count its
        // product attempt as product, never as a third skid: the census
        // retains every attempt's own class/signature and the rollup
        // reads those, not the first attempt copied forward.
        let mixed = RetriedCellV1 {
            path: "one/results.jsonl".into(),
            cell: CellIdentity {
                lane: "portable".into(),
                category: "fixture".into(),
                test: "fixture/mixed".into(),
                mode: "verify".into(),
                backend: "ptrace".into(),
            },
            attempts: 3,
            first_outcome: "ERROR".into(),
            first_failure_class: Some("understood_infrastructure_failure".into()),
            first_error_kind: Some("infrastructure".into()),
            first_signature: Some("skid_overshoot".into()),
            first_result: None,
            first_divergent_scheduler_turn: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            final_outcome: "FAIL".into(),
            attempt_outcomes: vec!["ERROR".into(), "ERROR".into(), "FAIL".into()],
            attempt_failure_classes: vec![
                Some("understood_infrastructure_failure".into()),
                Some("understood_infrastructure_failure".into()),
                Some("product_failure".into()),
            ],
            attempt_error_kinds: vec![
                Some("infrastructure".into()),
                Some("infrastructure".into()),
                None,
            ],
            attempt_signatures: vec![
                Some("skid_overshoot".into()),
                Some("skid_overshoot".into()),
                Some("unclassified".into()),
            ],
        };
        let census = FrameworkRetriesV1 {
            schema: 1,
            retried_cells: 1,
            retried_cells_final_pass: 0,
            cells: vec![mixed],
        };
        let rollup = RetryRollupV1::from_framework_retries(&census);
        assert_eq!(rollup.tests[0].attempt_outcomes, ["ERROR", "ERROR", "FAIL"]);
        assert_eq!(
            rollup
                .retries_by_class
                .get("understood_infrastructure_failure"),
            Some(&2)
        );
        assert_eq!(rollup.retries_by_class.get("product_failure"), Some(&1));
        assert_eq!(
            rollup.retries_by_subcategory["understood_infrastructure_failure"]["pmu_skid"],
            2
        );
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
