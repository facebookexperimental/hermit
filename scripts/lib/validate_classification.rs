// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Classify selected validation nodes without rewriting their raw evidence.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::AttemptExecution;
use super::NodeAttempt;
use super::StepOutcome;
use super::attempt_is_no_result;
use super::reported_attempt;
use super::validate_plan;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NodeClassification {
    Pass,
    ProductFailure,
    UnderstoodInfrastructureFailure,
    UnderstoodPrerequisiteFailure,
    NoResult,
}

impl NodeClassification {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::ProductFailure => "product_failure",
            Self::UnderstoodInfrastructureFailure => "understood_infrastructure_failure",
            Self::UnderstoodPrerequisiteFailure => "understood_prerequisite_failure",
            Self::NoResult => "no_result",
        }
    }

    pub(super) fn result(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::ProductFailure => "fail",
            Self::UnderstoodInfrastructureFailure
            | Self::UnderstoodPrerequisiteFailure
            | Self::NoResult => "no_result",
        }
    }

    pub(super) fn non_product_failure_bucket(self) -> Option<&'static str> {
        match self {
            Self::UnderstoodInfrastructureFailure => Some("understood_infrastructure_failure"),
            Self::UnderstoodPrerequisiteFailure => Some("understood_prerequisite_failure"),
            Self::NoResult => Some("no_result"),
            Self::Pass | Self::ProductFailure => None,
        }
    }

    fn is_product_result(self) -> bool {
        matches!(self, Self::Pass | Self::ProductFailure)
    }
}

/// Evidence of a failed condition remains authoritative alongside a diagnostic.
///
/// Test results were parsed from the controlled runner's structured report.
/// The refusal prefix is written by dagrun itself after rejecting that report
/// (`scheduler.rs::run_step`), rather than copied from the child's output.
pub(super) fn has_product_failure_evidence(attempt: &NodeAttempt) -> bool {
    attempt
        .test_results
        .as_ref()
        .is_some_and(|results| results.iter().any(|result| !result.passed))
        || attempt
            .reason
            .starts_with("STRUCTURED TEST RESULTS REFUSED: ")
}

pub(super) fn attempt_classification(attempt: &NodeAttempt) -> NodeClassification {
    if has_product_failure_evidence(attempt) {
        return NodeClassification::ProductFailure;
    }
    if !attempt.reported
        || attempt.execution != AttemptExecution::Completed
        || attempt.ok.is_none()
        || attempt.aborted
    {
        return NodeClassification::NoResult;
    }
    // A collected node budget breach remains a failed validation condition.
    // Its existing FailureClass::NoResult attribution and all termination
    // fields are retained separately; they are not rewritten into this view.
    if attempt.timed_out == Some(true)
        || attempt.cpu_timed_out == Some(true)
        || attempt.oomed == Some(true)
        || attempt.oom_kills.is_some_and(|kills| kills > 0)
    {
        return NodeClassification::ProductFailure;
    }
    if attempt.ok == Some(true) && attempt.returncode == Some(0) {
        return NodeClassification::Pass;
    }
    if attempt_is_no_result(attempt) {
        return NodeClassification::NoResult;
    }
    if attempt.failure_class == Some(super::FailureClass::UnderstoodPrerequisiteFailure) {
        return NodeClassification::UnderstoodPrerequisiteFailure;
    }
    if attempt.understood_infrastructure_class.is_some() {
        return NodeClassification::UnderstoodInfrastructureFailure;
    }
    NodeClassification::ProductFailure
}

/// The current producer has one outer attempt per node. Retained multiple-
/// attempt controls use the same fold: only a later actual pass recovers an
/// earlier failure, and an unknown or infrastructure attempt cannot erase a
/// recorded product failure.
pub(super) fn node_classification(
    outcome: &StepOutcome,
    attempts: &[NodeAttempt],
) -> NodeClassification {
    let node_attempts: Vec<_> = attempts
        .iter()
        .filter(|attempt| attempt.tag == outcome.tag)
        .collect();
    let Some(latest) = node_attempts.last() else {
        return attempt_classification(&reported_attempt(outcome, 1));
    };
    if attempt_classification(latest) == NodeClassification::Pass {
        return NodeClassification::Pass;
    }
    let classes: Vec<_> = node_attempts
        .iter()
        .map(|attempt| attempt_classification(attempt))
        .collect();
    if classes.contains(&NodeClassification::ProductFailure) {
        NodeClassification::ProductFailure
    } else if classes.contains(&NodeClassification::NoResult) {
        NodeClassification::NoResult
    } else if classes.contains(&NodeClassification::UnderstoodInfrastructureFailure) {
        NodeClassification::UnderstoodInfrastructureFailure
    } else if classes.contains(&NodeClassification::UnderstoodPrerequisiteFailure) {
        NodeClassification::UnderstoodPrerequisiteFailure
    } else {
        NodeClassification::NoResult
    }
}

#[derive(Debug, Default)]
pub(super) struct RunClassification {
    pub product_result_nodes: BTreeSet<String>,
    pub product_failure_nodes: BTreeSet<String>,
    pub understood_infrastructure_failure_nodes: BTreeMap<String, Vec<String>>,
    pub understood_prerequisite_failure_nodes: BTreeSet<String>,
    pub no_result_nodes: BTreeSet<String>,
}

impl RunClassification {
    pub(super) fn no_results(&self) -> usize {
        self.understood_infrastructure_failure_nodes.len()
            + self.understood_prerequisite_failure_nodes.len()
            + self.no_result_nodes.len()
    }

    pub(super) fn blocking_failures(&self, nonblocking: &BTreeSet<String>) -> usize {
        self.product_failure_nodes.difference(nonblocking).count()
    }

    pub(super) fn structural_failures(&self, nonblocking: &BTreeSet<String>) -> usize {
        self.product_failure_nodes
            .difference(nonblocking)
            .filter(|tag| !tag.starts_with("compat."))
            .count()
    }
}

pub(super) fn classify_run(
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
    skipped: &[String],
    planned_tags: &BTreeSet<String>,
    host_inapplicable: &[validate_plan::HostInapplicableNode],
) -> RunClassification {
    let mut classified = RunClassification::default();
    for outcome in outcomes {
        let classification = node_classification(outcome, attempts);
        if classification.is_product_result() {
            classified.product_result_nodes.insert(outcome.tag.clone());
        }
        match classification {
            NodeClassification::Pass => {}
            NodeClassification::ProductFailure => {
                classified.product_failure_nodes.insert(outcome.tag.clone());
            }
            NodeClassification::UnderstoodInfrastructureFailure => {
                let causes: BTreeSet<_> = attempts
                    .iter()
                    .filter(|attempt| attempt.tag == outcome.tag)
                    .filter_map(|attempt| attempt.understood_infrastructure_class.clone())
                    .collect();
                classified
                    .understood_infrastructure_failure_nodes
                    .insert(outcome.tag.clone(), causes.into_iter().collect());
            }
            NodeClassification::UnderstoodPrerequisiteFailure => {
                classified
                    .understood_prerequisite_failure_nodes
                    .insert(outcome.tag.clone());
            }
            NodeClassification::NoResult => {
                classified.no_result_nodes.insert(outcome.tag.clone());
            }
        }
    }
    classified
        .understood_prerequisite_failure_nodes
        .extend(host_inapplicable.iter().map(|node| node.tag.clone()));
    classified.no_result_nodes.extend(skipped.iter().cloned());
    let accounted: BTreeSet<_> = outcomes
        .iter()
        .map(|outcome| outcome.tag.as_str())
        .chain(skipped.iter().map(String::as_str))
        .chain(host_inapplicable.iter().map(|node| node.tag.as_str()))
        .collect();
    classified.no_result_nodes.extend(
        planned_tags
            .iter()
            .filter(|tag| !accounted.contains(tag.as_str()))
            .cloned(),
    );
    classified
}

pub(super) fn validation_is_complete(
    execution_complete: bool,
    classification: &RunClassification,
    planned_tags: &BTreeSet<String>,
) -> bool {
    execution_complete
        && classification.no_results() == 0
        && classification.product_result_nodes == *planned_tags
}

pub(super) fn validation_completeness_detail(
    validation_complete: bool,
    product_results: usize,
    selected: usize,
) -> String {
    if validation_complete {
        format!("COMPLETE: all {selected} selected node(s) produced a product result")
    } else {
        format!(
            "INCOMPLETE: {product_results} of {selected} selected node(s) produced a product \
             result; this run produced a partial answer and is not green evidence for landing"
        )
    }
}

/// Count only classified product results without fabricating StepOutcome fields.
/// The committed repetition count remains the selected denominator.
pub(super) fn stress_rates(
    classified: &RunClassification,
    reps: i64,
) -> Vec<super::validate_super::ProbeRate> {
    super::validate_super::STRESS_PROBES
        .iter()
        .map(|probe| {
            let prefix = format!("superstress.{}_", probe.slug().replace('-', "_"));
            let ran = classified
                .product_result_nodes
                .iter()
                .filter(|tag| tag.starts_with(&prefix))
                .count();
            let failed = classified
                .product_failure_nodes
                .iter()
                .filter(|tag| tag.starts_with(&prefix))
                .count();
            super::validate_super::ProbeRate {
                probe: *probe,
                passed: ran - failed,
                ran,
                planned: reps as usize,
            }
        })
        .collect()
}

fn fixture_outcome(tag: &str, exit: i64) -> StepOutcome {
    let mut outcome =
        StepOutcome::passed(tag.into(), 0.25, String::new(), Some(0), Some(3), Some(2));
    outcome.ok = exit == 0;
    outcome.returncode = Some(exit);
    outcome.reason = if exit == 0 {
        String::new()
    } else {
        "fixture condition failed".into()
    };
    outcome
}

fn fold_bracket() -> Result<(), String> {
    let pass = fixture_outcome("test.fold", 0);
    let fail = fixture_outcome("test.fold", 1);
    let first_fail = reported_attempt(&fail, 1);
    let mut infra = first_fail.clone();
    infra.understood_infrastructure_class = Some("bpfjailer-banner".into());
    let unknown = super::unreported_attempt(fail.tag.clone(), 2);
    for (label, raw, mut attempts, want) in [
        (
            "stale failure followed by pass",
            &fail,
            vec![first_fail.clone(), reported_attempt(&pass, 2)],
            NodeClassification::Pass,
        ),
        (
            "stale pass followed by failure",
            &pass,
            vec![reported_attempt(&pass, 1), reported_attempt(&fail, 2)],
            NodeClassification::ProductFailure,
        ),
        (
            "failure followed by unknown",
            &fail,
            vec![first_fail.clone(), unknown.clone()],
            NodeClassification::ProductFailure,
        ),
        (
            "failure followed by infrastructure",
            &fail,
            vec![first_fail.clone(), infra.clone()],
            NodeClassification::ProductFailure,
        ),
        (
            "infrastructure followed by unknown",
            &fail,
            vec![infra.clone(), unknown.clone()],
            NodeClassification::NoResult,
        ),
        (
            "infrastructure only",
            &fail,
            vec![infra.clone()],
            NodeClassification::UnderstoodInfrastructureFailure,
        ),
        (
            "pass followed by unknown",
            &pass,
            vec![reported_attempt(&pass, 1), unknown],
            NodeClassification::NoResult,
        ),
    ] {
        for (index, attempt) in attempts.iter_mut().enumerate() {
            attempt.attempt = index + 1;
        }
        let planned = BTreeSet::from([raw.tag.clone()]);
        let classified = classify_run(std::slice::from_ref(raw), &attempts, &[], &planned, &[]);
        let gate = super::ledger_gate_with_attempts(raw, &attempts);
        let failed = usize::from(want == NodeClassification::ProductFailure);
        let incomplete = !want.is_product_result();
        if node_classification(raw, &attempts) != want
            || gate["result"] != want.result()
            || classified.product_failure_nodes.len() != failed
            || classified.no_results() != usize::from(incomplete)
            || validation_is_complete(true, &classified, &planned) == incomplete
            || super::completed_exit_code(failed, classified.no_results(), false, false)
                != if failed > 0 {
                    1
                } else if incomplete {
                    super::NO_RESULT_EXIT_CODE as u8
                } else {
                    0
                }
            || raw.executed_tests != Some(3)
            || raw.filtered_tests != Some(2)
            || gate["attempts"].as_array().map(Vec::len) != Some(attempts.len())
        {
            return Err(format!(
                "classification fold {label}: wanted {want:?}; {classified:?}; gate={gate}"
            ));
        }
    }
    Ok(())
}

fn product_evidence_bracket() -> Result<(), String> {
    let mut outcome = fixture_outcome("test.mixed", 1);
    let mut infra = reported_attempt(&outcome, 1);
    infra.understood_infrastructure_class = Some("PMU RCB overshoot".into());
    if attempt_classification(&infra) != NodeClassification::UnderstoodInfrastructureFailure {
        return Err(
            "classification positive: complete infrastructure diagnosis was not recognized".into(),
        );
    }
    let failed_test = dagrun::TestResult::new("fixture::fails".into(), false, 1)?;
    outcome.test_results = Some(vec![failed_test]);
    let mut mixed = reported_attempt(&outcome, 1);
    mixed.understood_infrastructure_class = infra.understood_infrastructure_class.clone();
    if attempt_classification(&mixed) != NodeClassification::ProductFailure {
        return Err(
            "classification: infrastructure diagnostic erased the same node's measured failed test"
                .into(),
        );
    }
    let mut refused = infra.clone();
    refused.reason = "STRUCTURED TEST RESULTS REFUSED: duplicate test identity".into();
    if attempt_classification(&refused) != NodeClassification::ProductFailure {
        return Err(
            "classification: infrastructure diagnostic erased the controlled result-import refusal"
                .into(),
        );
    }
    for bits in 1_u8..8 {
        let mut limited = infra.clone();
        limited.timed_out = Some(bits & 1 != 0);
        limited.cpu_timed_out = Some(bits & 2 != 0);
        limited.oomed = Some(bits & 4 != 0);
        limited.oom_kills = Some(if bits & 4 != 0 { 2 } else { 0 });
        limited.failure_class = Some(super::FailureClass::NoResult);
        if attempt_classification(&limited) != NodeClassification::ProductFailure {
            return Err(format!(
                "classification: collected node limit {bits} was excused by its diagnostic"
            ));
        }
        let gate = super::ledger_gate_with_attempts(&outcome, &[limited]);
        if gate["result"] != "fail"
            || gate["failure_class"] != "product_failure"
            || gate["raw_failure_class"] != "no_result"
            || gate["timed_out"] != (bits & 1 != 0)
            || gate["cpu_timed_out"] != (bits & 2 != 0)
            || gate["oomed"] != (bits & 4 != 0)
        {
            return Err(format!(
                "classification: collected node limit {bits} lost its raw facts: {gate}"
            ));
        }
    }
    Ok(())
}

fn populations_bracket() -> Result<(), String> {
    let outcomes = [
        fixture_outcome("pass", 0),
        fixture_outcome("failure", 1),
        fixture_outcome("infra", 1),
        fixture_outcome("prerequisite", 127),
        fixture_outcome("no-result", 75),
    ];
    let mut attempts: Vec<_> = outcomes.iter().map(|o| reported_attempt(o, 1)).collect();
    attempts[2].understood_infrastructure_class = Some("bpfjailer-banner".into());
    attempts[3].failure_class = Some(super::FailureClass::UnderstoodPrerequisiteFailure);
    attempts[3].failure_detail = Some("command not found".into());
    let planned = [
        "pass",
        "failure",
        "infra",
        "prerequisite",
        "no-result",
        "dependency-skipped",
        "missing",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let classified = classify_run(
        &outcomes,
        &attempts,
        &["dependency-skipped".into()],
        &planned,
        &[],
    );
    if classified.product_result_nodes != BTreeSet::from(["pass".into(), "failure".into()])
        || classified.product_failure_nodes != BTreeSet::from(["failure".into()])
        || classified.understood_infrastructure_failure_nodes
            != BTreeMap::from([("infra".into(), vec!["bpfjailer-banner".into()])])
        || classified.understood_prerequisite_failure_nodes
            != BTreeSet::from(["prerequisite".into()])
        || classified.no_result_nodes
            != BTreeSet::from([
                "no-result".into(),
                "dependency-skipped".into(),
                "missing".into(),
            ])
        || classified.no_results() != 5
        || validation_is_complete(true, &classified, &planned)
    {
        return Err(format!(
            "classification selected populations changed: {classified:?}"
        ));
    }
    let row = super::ledger_gate_with_attempts(&outcomes[3], &attempts);
    if row["failure_class"] != "understood_prerequisite_failure"
        || row["result"] != "no_result"
        || !row["failure_origin"].is_null()
        || row.get("failed_substeps").is_some()
    {
        return Err(format!(
            "classification prerequisite authority changed: {row}"
        ));
    }
    // Missing super repetitions keep the committed denominator and never become
    // measured failures. Both raw and classified rates agree for complete input.
    let complete: Vec<_> = super::validate_super::STRESS_PROBES
        .iter()
        .map(|probe| {
            fixture_outcome(
                &format!("superstress.{}_1", probe.slug().replace('-', "_")),
                0,
            )
        })
        .collect();
    let planned = complete.iter().map(|outcome| outcome.tag.clone()).collect();
    let classes = classify_run(&complete, &[], &[], &planned, &[]);
    let old = super::validate_super::stress_rates(&complete, 1);
    let new = stress_rates(&classes, 1);
    if old.iter().zip(&new).any(|(a, b)| {
        (a.probe, a.passed, a.ran, a.planned) != (b.probe, b.passed, b.ran, b.planned)
    }) {
        return Err("classification super rates changed a complete product population".into());
    }
    let partial = stress_rates(&classes, 2);
    if partial
        .iter()
        .any(|rate| (rate.passed, rate.ran, rate.planned) != (1, 1, 2))
        || super::print_super_stress_verdict(&partial, 2, 1, 1) != 0
    {
        return Err("classification super rates invented a failed repetition".into());
    }
    let mut failed = classes;
    failed.product_failure_nodes.insert(complete[0].tag.clone());
    if super::print_super_stress_verdict(&stress_rates(&failed, 2), 2, 1, 1) != 1 {
        return Err("classification super rates lost a measured blocking failure".into());
    }
    Ok(())
}

pub(super) fn self_test() -> Result<String, String> {
    fold_bracket()?;
    product_evidence_bracket()?;
    populations_bracket()?;
    Ok("classification: exact selected populations; stale/raw fold agreement; failed tests and node limits outrank infrastructure; missing super repetitions remain unmeasured".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_verdict_and_raw_attempts_remain_separate() {
        fold_bracket().unwrap();
    }
    #[test]
    fn measured_product_evidence_outranks_infrastructure() {
        product_evidence_bracket().unwrap();
    }
    #[test]
    fn selected_populations_and_super_denominators_are_exact() {
        populations_bracket().unwrap();
    }

    #[test]
    fn actual_structured_test_failure_and_refusal_outrank_same_node_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let diagnostic = "An action was blocked on this server based on a security policy!\nEnforcer: FS, Reason: FILE_OPEN";
        for (name, report, wanted) in [
            (
                "mixed",
                Some(
                    r#"{"schema":2,"executed_tests":1,"filtered_tests":2,"results":[{"id":"fixture::fails","result":"fail","attempts":1}]}"#,
                ),
                NodeClassification::ProductFailure,
            ),
            (
                "refused",
                Some(r#"{"schema":2,"executed_tests":1,"filtered_tests":2,"results":[]}"#),
                NodeClassification::ProductFailure,
            ),
            (
                "infrastructure",
                None,
                NodeClassification::UnderstoodInfrastructureFailure,
            ),
        ] {
            let detail = dir.path().join(format!("{name}.detail"));
            let command = format!(
                "printf '%s\\n' {} > {}; cat {}; {} exit 1",
                validate_plan::shell_quote(diagnostic),
                validate_plan::shell_quote(&detail.to_string_lossy()),
                validate_plan::shell_quote(&detail.to_string_lossy()),
                report
                    .map(|body| format!(
                        "printf '%s\\n' {} > \"$DAGRUN_TEST_COUNTS_PATH\";",
                        validate_plan::shell_quote(body)
                    ))
                    .unwrap_or_default()
            );
            let mut step = super::super::step_with_caps(
                "classification",
                name,
                "actual structured-result classification fixture",
                command,
                Vec::new(),
                10,
                10,
                128 * 1024 * 1024,
            );
            if report.is_some() {
                step.result_manifests =
                    Some(vec![dagrun::model::ResultManifest::StructuredTestResults(
                        dagrun::model::StructuredTestResultsManifest::current(step.tag()),
                    )]);
            }
            let cfg = super::super::DagConfig {
                steps: vec![step],
                ..Default::default()
            };
            let result = super::super::run_lane_once(
                &cfg,
                1,
                true,
                0,
                None,
                &dir.path().join(format!("{name}.log")),
                None,
                false,
            );
            assert_eq!(result.outcomes.len(), 1, "{name}: exact collected result");
            assert_eq!(result.attempts.len(), 1, "{name}: no added outer retry");
            let outcome = &result.outcomes[0];
            assert_eq!(outcome.returncode, Some(1));
            assert!(
                !outcome.ok
                    && !outcome.aborted
                    && !outcome.timed_out
                    && !outcome.cpu_timed_out
                    && !outcome.oomed
            );
            let captured = std::fs::read_to_string(&detail).unwrap();
            assert_eq!(captured, format!("{diagnostic}\n"));
            let mut attempts = result.attempts;
            super::super::stamp_attempt_detail(
                &mut attempts,
                &outcome.tag,
                super::super::validate_runtime::environmental_block_class(&captured),
                super::super::validate_runtime::understood_infrastructure_class(&captured),
                super::super::validate_runtime::failure_class_from_detail(&captured),
            );
            assert_eq!(
                node_classification(outcome, &attempts),
                wanted,
                "{name}: same-node precedence"
            );
            if name == "mixed" {
                assert_eq!(
                    (outcome.executed_tests, outcome.filtered_tests),
                    (Some(1), Some(2))
                );
                assert_eq!(
                    outcome.test_results.as_ref().unwrap(),
                    &vec![dagrun::TestResult::new("fixture::fails".into(), false, 1).unwrap()]
                );
            } else if name == "refused" {
                assert!(
                    outcome
                        .reason
                        .starts_with("STRUCTURED TEST RESULTS REFUSED: ")
                );
                assert!(outcome.test_results.is_none() && outcome.executed_tests.is_none());
            }
            let gate = super::super::ledger_gate_with_attempts(outcome, &attempts);
            assert_eq!(gate["result"], wanted.result());
            assert_eq!(gate["failure_class"], wanted.as_str());
            println!("actual classification {name}: {gate}");
        }
    }
}

#[cfg(test)]
mod ledger_tests {
    use super::*;

    #[test]
    fn actual_ledger_rows_bind_aggregate_results_and_preserve_raw_observations() {
        let dir = tempfile::tempdir().unwrap();
        let pass = fixture_outcome("test.fixture", 0);
        let fail = fixture_outcome("test.fixture", 1);
        let planned = BTreeSet::from([pass.tag.clone()]);
        let mut aborted = fail.clone();
        aborted.aborted = true;
        aborted.returncode = Some(-15);
        let mut budget = fail.clone();
        budget.cpu_timed_out = true;
        budget.returncode = Some(-15);
        budget.reason = "typed CPU timeout fixture".into();
        let mut infrastructure = reported_attempt(&fail, 1);
        infrastructure.understood_infrastructure_class = Some("bpfjailer-banner".into());
        let mut prerequisite = reported_attempt(&fail, 1);
        prerequisite.failure_class =
            Some(super::super::FailureClass::UnderstoodPrerequisiteFailure);
        prerequisite.failure_detail = Some("required fixture compiler is unavailable".into());
        for (name, outcomes, attempts, complete, exit, result, failures) in [
            (
                "pass",
                vec![pass.clone()],
                vec![reported_attempt(&pass, 1)],
                true,
                0,
                "pass",
                0,
            ),
            (
                "product_after_unknown",
                vec![fail.clone()],
                vec![
                    reported_attempt(&fail, 1),
                    super::super::unreported_attempt(fail.tag.clone(), 2),
                ],
                false,
                1,
                "fail",
                1,
            ),
            (
                "product_after_aborted",
                vec![fail.clone()],
                vec![reported_attempt(&fail, 1), reported_attempt(&aborted, 2)],
                false,
                1,
                "fail",
                1,
            ),
            (
                "whole_run_cutoff",
                vec![],
                vec![],
                false,
                super::super::NO_RESULT_EXIT_CODE as u8,
                "no_result",
                0,
            ),
            (
                "node_budget",
                vec![budget.clone()],
                vec![reported_attempt(&budget, 1)],
                true,
                1,
                "fail",
                1,
            ),
            (
                "infrastructure",
                vec![fail.clone()],
                vec![infrastructure],
                true,
                super::super::NO_RESULT_EXIT_CODE as u8,
                "no_result",
                0,
            ),
            (
                "prerequisite",
                vec![fail.clone()],
                vec![prerequisite],
                true,
                super::super::NO_RESULT_EXIT_CODE as u8,
                "no_result",
                0,
            ),
        ] {
            let ctx = super::super::LedgerCtx {
                started_at: "2026-09-13T00:00:00Z".into(),
                host: "classification-fixture".into(),
                toolchain: "fixture".into(),
                slot: "fixture".into(),
                cwd: dir.path().display().to_string(),
                profile: "full".into(),
                selection_mode: "full".into(),
                cache_state: "warm".into(),
                commit: "535b48a113390f0084eac204b54de67dacc24f29".into(),
                tree: "d0fc13f45eb671585d1aa26c7fcf3fdb4ca480c7".into(),
                git_depth: 1,
                git_ahead: 0,
                git_behind: 0,
                commit_anchored: false,
                tree_dirty: false,
                dag_jobs: 1,
                admission: None,
                base_sha: serde_json::Value::Null,
                base_tree: serde_json::Value::Null,
                reverie_base_sha: serde_json::Value::Null,
                reverie_base_tree: serde_json::Value::Null,
                reverie_pin_current: false,
                concurrent_validates: None,
                concurrency_proof: None,
                interruption: None,
                cpu_user: 0.0,
                cpu_sys: 0.0,
                retry_rounds: 0,
                executed_tests: if outcomes.is_empty() { None } else { Some(3) },
                passed_tests: if outcomes.is_empty() {
                    None
                } else {
                    Some(if failures == 0 { 3 } else { 2 })
                },
                filtered_tests: if outcomes.is_empty() { None } else { Some(2) },
            };
            let path = dir.path().join(format!("{name}.jsonl"));
            super::super::write_ledger(
                &path,
                &ctx,
                &outcomes,
                &attempts,
                &[],
                &[],
                &planned,
                0.25,
                exit,
                "fixture.log",
                complete,
                serde_json::json!({}),
                None,
            );
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text.lines().count(), 1);
            let row: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(row["result"], result, "{name}");
            assert_eq!(row["failures"], failures, "{name}");
            assert_eq!(row["gates_expected"], 1, "{name}");
            assert_eq!(row["checks"], outcomes.len(), "{name}");
            assert_eq!(
                row["executed_tests"],
                serde_json::json!(ctx.executed_tests),
                "{name}"
            );
            assert_eq!(
                row["passed_tests"],
                serde_json::json!(ctx.passed_tests),
                "{name}"
            );
            assert_eq!(
                row["filtered_tests"],
                serde_json::json!(ctx.filtered_tests),
                "{name}"
            );
            assert_eq!(
                row["validation_complete"],
                matches!(name, "pass" | "node_budget"),
                "{name}"
            );
            if name == "product_after_unknown" {
                assert_eq!(row["gates"][0]["result"], "fail");
                assert_eq!(row["gates"][0]["failure_class"], "product_failure");
                assert_eq!(row["gates"][0]["failure_origin"], "outer_gate");
                assert_eq!(row["gates"][0]["failed_substeps"], serde_json::json!([]));
                assert!(row["gates"][0]["raw_result"].is_null());
                assert_eq!(row["gates"][0]["raw_failure_class"], "no_result");
            }
            if name == "product_after_aborted" {
                assert_eq!(row["gates"][0]["aborted"], false);
                assert_eq!(row["gates"][0]["raw_aborted"], true);
                assert_eq!(row["gates"][0]["attempts"][1]["aborted"], true);
                assert_eq!(row["gates"][0]["failure_origin"], "outer_gate");
                assert_eq!(row["gates"][0]["failure_class"], "product_failure");
            }
            if name == "whole_run_cutoff" {
                assert_eq!(row["no_result_nodes"], serde_json::json!(["test.fixture"]));
                assert_eq!(row["product_result_nodes"], serde_json::json!([]));
                assert_eq!(row["gates"], serde_json::json!([]));
            }
            // These are deliberately nonqualifying fixture rows: no fabricated
            // clean-source, preflight, concurrency, or test-coverage authority.
            assert_eq!(row["commit_anchored"], false);
            assert!(row["admission"].is_null());
            println!("SERIALIZER_FIXTURE {name} {row}");
        }
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn actual_run_cutoff_is_incomplete_while_collected_node_timeout_stays_red() {
        let dir = tempfile::tempdir().unwrap();
        let make_step = |job: &str, command: &str, wall: i64| {
            super::super::step_with_caps(
                "classification",
                job,
                "actual timeout classification fixture",
                command.into(),
                Vec::new(),
                wall,
                10,
                128 * 1024 * 1024,
            )
        };
        let limited = make_step("node_limit", "sleep 4", 1);
        let cfg = super::super::DagConfig {
            steps: vec![limited],
            ..Default::default()
        };
        let result = super::super::run_lane_once(
            &cfg,
            1,
            true,
            0,
            None,
            &dir.path().join("node-limit.log"),
            None,
            false,
        );
        assert_eq!(result.outcomes.len(), 1);
        assert!(!result.run_timed_out);
        assert!(result.outcomes[0].timed_out);
        let planned = BTreeSet::from(["classification.node_limit".to_string()]);
        let classified = classify_run(
            &result.outcomes,
            &result.attempts,
            &result.skipped,
            &planned,
            &[],
        );
        assert_eq!(classified.product_failure_nodes, planned);
        assert_eq!(
            super::super::completed_exit_code(
                classified.blocking_failures(&BTreeSet::new()),
                classified.no_results(),
                result.run_timed_out,
                false
            ),
            1
        );
        assert_eq!(result.outcomes[0].executed_tests, None);
        println!("actual collected node limit: {:?}", result.outcomes);

        for prior_failure in [false, true] {
            let mut pending = make_step("pending", "true", 10);
            pending.deps = vec!["classification.cutoff".into()];
            let mut steps = Vec::new();
            if prior_failure {
                steps.push(make_step("prior_failure", "exit 1", 10));
            }
            steps.extend([make_step("cutoff", "sleep 10", 10), pending]);
            let cfg = super::super::DagConfig {
                steps,
                ..Default::default()
            };
            let planned = cfg.steps.iter().map(|step| step.tag()).collect();
            let deadline = super::super::monotonic_now_ns().unwrap() + 2_000_000_000;
            let result = super::super::run_lane_once(
                &cfg,
                1,
                true,
                0,
                None,
                &dir.path().join(format!("cutoff-{prior_failure}.log")),
                Some(deadline),
                false,
            );
            assert!(result.run_timed_out && !result.complete);
            assert!(
                !result
                    .outcomes
                    .iter()
                    .any(|outcome| outcome.tag == "classification.pending")
            );
            let classified = classify_run(
                &result.outcomes,
                &result.attempts,
                &result.skipped,
                &planned,
                &[],
            );
            assert!(classified.no_results() >= 2);
            assert_eq!(
                classified.product_failure_nodes,
                if prior_failure {
                    BTreeSet::from(["classification.prior_failure".into()])
                } else {
                    BTreeSet::new()
                }
            );
            assert!(!validation_is_complete(
                result.complete,
                &classified,
                &planned
            ));
            let exit = super::super::completed_exit_code(
                classified.blocking_failures(&BTreeSet::new()),
                classified.no_results(),
                result.run_timed_out,
                false,
            );
            assert_eq!(
                exit,
                if prior_failure {
                    1
                } else {
                    super::super::NO_RESULT_EXIT_CODE as u8
                }
            );
            assert_eq!(
                super::super::exit_code_with_execution_completeness(exit, false),
                exit
            );
            println!(
                "actual whole-run cutoff prior_failure={prior_failure}: {classified:?}; outcomes={:?}",
                result.outcomes
            );
        }
    }
}
