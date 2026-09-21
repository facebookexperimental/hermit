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

/// dagrun funnels three distinct conditions into one refusal, and they do not
/// carry the same evidence. `cannot read structured test results {path}: ...`
/// and `malformed structured test results {path}: ...` each describe a report
/// the run actually produced, so the suite is a fair suspect -- a duplicate
/// test identity is a malformed suite. Only the constant below says no report
/// exists, which is an absence of evidence in either direction.
const RESULTS_NEVER_WRITTEN: &str = "required structured test results were not written";
const REFUSAL_REASON_PREFIX: &str = "STRUCTURED TEST RESULTS REFUSED: ";

/// The refusal the producer recorded, if any. `test_results_error` is the
/// current typed field; the reason prefix is the older contract, retained
/// because attempts recorded under it are still read back.
fn refusal_cause(attempt: &NodeAttempt) -> Option<&str> {
    attempt
        .test_results_error
        .as_deref()
        .or_else(|| attempt.reason.strip_prefix(REFUSAL_REASON_PREFIX))
}

/// Matched as a PREFIX rather than a substring deliberately. The other two
/// causes interpolate a path, so a path that happens to contain this phrase
/// must not be mistaken for a report that was never written.
fn refused_without_writing_any_report(attempt: &NodeAttempt) -> bool {
    refusal_cause(attempt).is_some_and(|cause| cause.starts_with(RESULTS_NEVER_WRITTEN))
}

/// The one case with no evidence in either direction: the node's own command
/// succeeded, and the only thing marking it not-ok is a report that was never
/// written. Nothing failed that anybody measured.
///
/// The command's own status is tested here rather than inferred from where
/// this is called. A node that exited nonzero DID fail a condition; a missing
/// report means that failure cannot be NAMED, and naming it is not the same as
/// it existing. Collected wall/CPU/OOM breaches are likewise real failed
/// conditions and are excluded for the same reason.
fn absent_report_is_the_only_failure(attempt: &NodeAttempt) -> bool {
    attempt.ok == Some(false)
        && attempt.returncode == Some(0)
        // An authoritative invalid/read-I/O kind must not be excused by a
        // contradictory legacy string that happens to begin with this phrase.
        && matches!(
            attempt.test_results_error_kind,
            None | Some(dagrun::TestResultsErrorKind::Missing)
        )
        && attempt.timed_out != Some(true)
        && attempt.cpu_timed_out != Some(true)
        && attempt.oomed != Some(true)
        && !attempt.oom_kills.is_some_and(|kills| kills > 0)
        && refused_without_writing_any_report(attempt)
}

/// A controlled producer uses the existing temporary-failure status only after
/// valid test evidence could not be written/published. The runner independently
/// observed absence/read I/O; prose and a generic exit 2 cannot grant this class.
fn publication_report_is_unavailable(attempt: &NodeAttempt) -> bool {
    attempt.ok == Some(false)
        && attempt.returncode == Some(super::NO_RESULT_EXIT_CODE)
        && attempt.test_results_error.is_some()
        && matches!(
            attempt.test_results_error_kind,
            Some(dagrun::TestResultsErrorKind::Missing | dagrun::TestResultsErrorKind::ReadIo)
        )
        && attempt.timed_out != Some(true)
        && attempt.cpu_timed_out != Some(true)
        && attempt.oomed != Some(true)
        && !attempt.oom_kills.is_some_and(|kills| kills > 0)
}

/// Evidence of a failed condition remains authoritative alongside a diagnostic.
///
/// Test results were parsed from the controlled runner's structured report.
/// Current dagrun retains a refused required report in `test_results_error`,
/// separately from the exit reason. The legacy reason prefix is also retained.
/// A refusal alone cannot turn an uncollected or aborted attempt into a result.
pub(super) fn has_product_failure_evidence(attempt: &NodeAttempt) -> bool {
    if attempt
        .test_results
        .as_ref()
        .is_some_and(|results| results.iter().any(|result| !result.passed))
    {
        return true;
    }
    attempt.reported
        && attempt.execution == AttemptExecution::Completed
        && attempt.ok.is_some()
        && !attempt.aborted
        && (refusal_cause(attempt).is_some() || attempt.test_results_error_kind.is_some())
        && !absent_report_is_the_only_failure(attempt)
        && !publication_report_is_unavailable(attempt)
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
    // Last, so that every measured condition above still wins: a node whose
    // command succeeded and whose required report was never written produced
    // no evidence either way. `no_result` keeps it out of the failure count
    // and equally out of any green evidence for landing, which is both halves
    // of the ruling. It is deliberately NOT UnderstoodInfrastructureFailure:
    // nothing here diagnosed a cause, and that bucket is projected with a
    // named cause it would have to leave empty.
    if absent_report_is_the_only_failure(attempt) {
        return NodeClassification::NoResult;
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

/// A report that was never written is an absence of evidence, not a failure --
/// and every other measured condition still outranks that absence.
///
/// The fixture deliberately starts from a plain failing outcome with NO
/// infrastructure diagnosis, so the only thing that can satisfy the
/// never-written expectations is this reclassification. Cloning the `infra`
/// fixture instead would reach `no_result` through
/// `understood_infrastructure_class` and the assertions could not fail.
fn absent_report_bracket() -> Result<(), String> {
    let clean = fixture_outcome("test.absent", 0);
    // The producer marks any refusal not-ok while leaving the command's own
    // zero exit status in place. That pairing is the whole subject here.
    let refused_only = |cause: &str| {
        let mut attempt = reported_attempt(&clean, 1);
        attempt.ok = Some(false);
        attempt.test_results_error = Some(cause.into());
        attempt
    };
    let never_written =
        "required structured test results were not written to /src/.dagrun-test-counts-x.json";
    let malformed = "malformed structured test results /src/counts.json: \
                     structured-test-results-results has 0 terminal row(s), expected exactly 1 \
                     executed test(s)";
    let unreadable = "cannot read structured test results /src/counts.json: \
                      No such file or directory (os error 2)";

    if attempt_classification(&refused_only(never_written)) != NodeClassification::NoResult {
        return Err(
            "classification: a report that was never written was reported as a product failure"
                .into(),
        );
    }
    for (label, cause) in [("malformed", malformed), ("unreadable", unreadable)] {
        if attempt_classification(&refused_only(cause)) != NodeClassification::ProductFailure {
            return Err(format!(
                "classification: a {label} report stopped being product evidence"
            ));
        }
    }
    // The phrase inside an interpolated PATH must not be mistaken for absence.
    // This is why the match is a prefix and not a substring.
    let phrase_in_path = format!(
        "malformed structured test results \
         /src/{RESULTS_NEVER_WRITTEN}/counts.json: trailing characters at line 1"
    );
    if attempt_classification(&refused_only(&phrase_in_path)) != NodeClassification::ProductFailure
    {
        return Err(
            "classification: a malformed report whose path contains the absence phrase was \
             excused"
                .into(),
        );
    }
    // The legacy reason contract, which older attempts are still read back with.
    for (label, cause, want) in [
        ("never written", never_written, NodeClassification::NoResult),
        ("malformed", malformed, NodeClassification::ProductFailure),
    ] {
        let mut legacy = reported_attempt(&clean, 1);
        legacy.ok = Some(false);
        legacy.reason = format!("{REFUSAL_REASON_PREFIX}{cause}");
        if attempt_classification(&legacy) != want {
            return Err(format!(
                "classification: legacy reason form of a {label} report changed meaning"
            ));
        }
    }
    // A measured failing test outranks the absence of the report it came in.
    let mut with_failed_test = refused_only(never_written);
    with_failed_test.test_results = Some(vec![dagrun::TestResult::new(
        "fixture::fails".into(),
        false,
        1,
    )?]);
    if attempt_classification(&with_failed_test) != NodeClassification::ProductFailure {
        return Err(
            "classification: an absent report erased the same node's measured failed test".into(),
        );
    }
    // A command that failed on its own DID fail a condition; only the naming
    // is lost. Every nonzero status stays a product failure.
    for code in [1_i64, 2, 101, -15] {
        let mut failed_command = refused_only(never_written);
        failed_command.returncode = Some(code);
        failed_command.reason = "exit 1".into();
        if attempt_classification(&failed_command) != NodeClassification::ProductFailure {
            return Err(format!(
                "classification: an absent report excused a command that exited {code}"
            ));
        }
    }
    // Collected node-limit breaches are measured failed conditions and keep
    // their precedence even when the report is missing.
    for bits in 1_u8..8 {
        let mut limited = refused_only(never_written);
        limited.timed_out = Some(bits & 1 != 0);
        limited.cpu_timed_out = Some(bits & 2 != 0);
        limited.oomed = Some(bits & 4 != 0);
        limited.oom_kills = Some(if bits & 4 != 0 { 2 } else { 0 });
        limited.failure_class = Some(super::FailureClass::NoResult);
        if attempt_classification(&limited) != NodeClassification::ProductFailure {
            return Err(format!(
                "classification: an absent report excused collected node limit {bits}"
            ));
        }
    }
    // An attempt that never completed was already unknown and stays unknown;
    // the absence must not upgrade it into a diagnosed infrastructure cause.
    for bits in 1_u8..16 {
        let mut incomplete = refused_only(never_written);
        incomplete.reported = bits & 1 == 0;
        if bits & 2 != 0 {
            incomplete.execution = AttemptExecution::Unknown;
        }
        incomplete.aborted = bits & 4 != 0;
        if bits & 8 != 0 {
            incomplete.ok = None;
        }
        if attempt_classification(&incomplete) != NodeClassification::NoResult {
            return Err(format!(
                "classification: incomplete attempt {bits} with an absent report left no_result"
            ));
        }
    }
    // A real infrastructure diagnosis is more informative than "cannot tell",
    // so it keeps its own bucket rather than being flattened into no_result.
    let mut diagnosed = refused_only(never_written);
    diagnosed.understood_infrastructure_class = Some("PMU RCB overshoot".into());
    if attempt_classification(&diagnosed) != NodeClassification::UnderstoodInfrastructureFailure {
        return Err(
            "classification: an absent report erased a real infrastructure diagnosis".into(),
        );
    }
    // The projection a consumer actually reads, and it must not invent a cause.
    let gate = super::ledger_gate_with_attempts(&clean, &[refused_only(never_written)]);
    if gate["result"] != "no_result" || gate["failure_class"] != "no_result" {
        return Err(format!(
            "classification: absent-report projection did not read as no_result; gate={gate}"
        ));
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
    let mut typed_refusal = infra.clone();
    typed_refusal.test_results_error = Some("required report contained a duplicate test ID".into());
    if attempt_classification(&typed_refusal) != NodeClassification::ProductFailure
        || typed_refusal.reason != infra.reason
    {
        return Err(
            "classification: typed refusal lost precedence or changed the exit reason".into(),
        );
    }
    // A declared report refused after completion is different from an attempt
    // that never completed. Even a refusal diagnostic cannot fill that gap.
    for bits in 1_u8..16 {
        let mut incomplete = typed_refusal.clone();
        incomplete.reported = bits & 1 == 0;
        if bits & 2 != 0 {
            incomplete.execution = AttemptExecution::Unknown;
        }
        incomplete.aborted = bits & 4 != 0;
        if bits & 8 != 0 {
            incomplete.ok = None;
        }
        if attempt_classification(&incomplete) != NodeClassification::NoResult {
            return Err(format!(
                "classification: incomplete attempt {bits} acquired a result from a refusal"
            ));
        }
        incomplete.test_results = mixed.test_results.clone();
        if attempt_classification(&incomplete) != NodeClassification::ProductFailure {
            return Err(format!(
                "classification: incomplete attempt {bits} erased an accepted failed test"
            ));
        }
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
    let host = validate_plan::HostInapplicableNode {
        tag: "host.kvm".into(),
        capability: validate_plan::HostCapability::Kvm,
        evidence: "fixture host lacks /dev/kvm".into(),
    };
    let host_plan = BTreeSet::from([host.tag.clone()]);
    let unavailable = classify_run(&[], &[], &[], &host_plan, &[host]);
    if unavailable.understood_prerequisite_failure_nodes != host_plan
        || unavailable.no_results() != 1
        || validation_is_complete(true, &unavailable, &host_plan)
    {
        return Err(
            "classification: host-inapplicable selection was lost or promoted to completion".into(),
        );
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
    let empty_rates = stress_rates(&RunClassification::default(), 2);
    if empty_rates
        .iter()
        .any(|rate| (rate.passed, rate.ran, rate.planned) != (0, 0, 2))
        || super::print_super_stress_verdict(&empty_rates, 2, 1, 1) != 0
        || super::print_super_stress_verdict(&new, 1, 1, 1) != 0
    {
        return Err("classification super rates changed absent or complete populations".into());
    }
    let mut nonblocking = RunClassification::default();
    for probe in super::validate_super::STRESS_PROBES
        .iter()
        .filter(|probe| probe.nonblocking())
    {
        let tag = format!("superstress.{}_1", probe.slug().replace('-', "_"));
        nonblocking.product_result_nodes.insert(tag.clone());
        nonblocking.product_failure_nodes.insert(tag);
    }
    if super::print_super_stress_verdict(&stress_rates(&nonblocking, 2), 2, 1, 1) != 0 {
        return Err(
            "classification super rates changed the existing nonblocking probe policy".into(),
        );
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
    absent_report_bracket()?;
    populations_bracket()?;
    Ok("classification: exact selected populations; stale/raw fold agreement; failed tests and node limits outrank infrastructure; an unwritten required report is undetermined rather than failed; missing super repetitions remain unmeasured".into())
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
    fn an_unwritten_report_is_undetermined_and_never_outranks_a_measured_condition() {
        absent_report_bracket().unwrap();
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
                let refusal = outcome.test_results_error.as_deref().unwrap();
                let (cause, diagnostic_tail) = refusal
                    .split_once("; last output: ")
                    .expect("refusal must retain the step's diagnostic");
                assert!(
                    cause.starts_with("malformed structured test results ")
                        && cause.ends_with(
                            ": structured-test-results-results has 0 terminal row(s), expected exactly 1 executed test(s)"
                        ),
                    "{refusal}"
                );
                assert_eq!(
                    diagnostic_tail,
                    "An action was blocked on this server based on a security policy! | Enforcer: FS, Reason: FILE_OPEN"
                );
                assert_eq!(outcome.reason, "exit 1");
                assert_eq!(attempts[0].test_results_error, outcome.test_results_error);
                assert!(outcome.test_results.is_none() && outcome.executed_tests.is_none());
            }
            let gate = super::super::ledger_gate_with_attempts(outcome, &attempts);
            assert_eq!(gate["result"], wanted.result());
            assert_eq!(gate["failure_class"], wanted.as_str());
            println!("actual classification {name}: {gate}");
        }
    }

    /// The same condition through the REAL producer rather than a hand-built
    /// attempt, because the reclassification keys on the exact wording dagrun
    /// emits and reading that wording out of its source is not the same as
    /// observing it. This runs a genuine lane that declares a required report
    /// and then does not write one.
    ///
    /// The `wrote` case is the control that makes the other one mean something:
    /// the identical step, differing only in whether the report is produced,
    /// passes. So the absence is the single variable.
    #[test]
    fn a_real_lane_that_writes_no_required_report_is_undetermined_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        for (name, write_report, wanted) in [
            ("absent", false, NodeClassification::NoResult),
            ("wrote", true, NodeClassification::Pass),
        ] {
            // Exits 0 either way. A command that failed on its own would be a
            // product failure regardless, which is a different question.
            let command = if write_report {
                "printf '%s\\n' '{\"schema\":2,\"executed_tests\":1,\"filtered_tests\":0,\
                 \"results\":[{\"id\":\"fixture::passes\",\"result\":\"pass\",\"attempts\":1}]}' \
                 > \"$DAGRUN_TEST_COUNTS_PATH\"; exit 0"
                    .to_string()
            } else {
                "exit 0".to_string()
            };
            let mut step = super::super::step_with_caps(
                "classification",
                name,
                "actual absent-required-report fixture",
                command,
                Vec::new(),
                10,
                10,
                128 * 1024 * 1024,
            );
            step.result_manifests =
                Some(vec![dagrun::model::ResultManifest::StructuredTestResults(
                    dagrun::model::StructuredTestResultsManifest::current(step.tag()),
                )]);
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
            let outcome = &result.outcomes[0];
            let attempts = result.attempts;
            assert_eq!(outcome.returncode, Some(0), "{name}: command exited 0");
            assert!(
                !outcome.aborted && !outcome.timed_out && !outcome.cpu_timed_out && !outcome.oomed,
                "{name}: no collected limit breach"
            );
            if write_report {
                // The control has to prove the lane really produced and parsed
                // a report, otherwise "it passed" says nothing about whether
                // the absence in the other case was the variable.
                assert!(outcome.ok && outcome.test_results_error.is_none());
                assert_eq!(outcome.executed_tests, Some(1), "{name}: report parsed");
                assert_eq!(
                    outcome.test_results.as_ref().unwrap(),
                    &vec![dagrun::TestResult::new("fixture::passes".into(), true, 1).unwrap()],
                    "{name}: the written row was read back"
                );
            } else {
                // The exact producer wording the classifier keys on, observed
                // rather than quoted from the producer's source.
                let refusal = outcome.test_results_error.as_deref().unwrap();
                assert!(
                    refusal.starts_with(RESULTS_NEVER_WRITTEN),
                    "producer wording moved out from under the classifier: {refusal}"
                );
                assert!(!outcome.ok, "{name}: a refusal still marks the node not-ok");
                assert!(outcome.test_results.is_none());
            }
            assert_eq!(
                node_classification(outcome, &attempts),
                wanted,
                "{name}: absent required report"
            );
            // The projection a consumer reads. A passing lane records no
            // attempt, so `ledger_gate_with_attempts` leaves the per-attempt
            // fields unset there; the refusal is the case that must project.
            if !write_report {
                let gate = super::super::ledger_gate_with_attempts(outcome, &attempts);
                assert_eq!(gate["result"], wanted.result());
                assert_eq!(gate["failure_class"], wanted.as_str());
                // The raw observation is preserved beside the verdict rather
                // than rewritten by it.
                assert_eq!(gate["raw_result"], "fail");
                assert_eq!(gate["raw_failure_class"], "product_failure");
                assert_eq!(gate["exit_code"], 0);
                println!("actual absent-report classification {name}: {gate}");
            }
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
                "incomplete_after_collected_pass",
                vec![pass.clone()],
                vec![reported_attempt(&pass, 1)],
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
                run_id: std::env::var("E2E_RUN_ID").ok(),
                admission_floor_evidence: None,
                admission_provenance_error: None,
                log_identity: None,
                base_observation: serde_json::Value::Null,
                main_observation: serde_json::Value::Null,
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
                git_ahead: Some(0),
                git_behind: Some(0),
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
            if name == "incomplete_after_collected_pass" {
                assert_eq!(row["raw_result"], "fail");
                assert_eq!(row["exit_code"], super::super::NO_RESULT_EXIT_CODE);
                assert_eq!(row["no_result_nodes"], serde_json::json!([]));
                assert_eq!(
                    row["product_result_nodes"],
                    serde_json::json!(["test.fixture"])
                );
                assert_eq!(row["gates"][0]["result"], "pass");
                assert_eq!(row["validation_complete"], false);
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

        for (prior_failure, cold_log) in [(false, true), (false, false), (true, false)] {
            let mut steps = Vec::new();
            if cold_log {
                // Keep the exact original failing 2 s / sleep 10 input.
                let mut pending = make_step("pending", "true", 10);
                pending.deps = vec!["classification.cutoff".into()];
                steps.extend([make_step("cutoff", "sleep 10", 10), pending]);
            } else {
                // The runner correctly refuses an individual node whose own
                // bound cannot fit the remaining run budget. Exercise an active
                // whole-run cutoff using three individually bounded steps whose
                // sequential total exceeds that allowance instead.
                if prior_failure {
                    steps.push(make_step("prior_failure", "exit 1", 2));
                }
                let first = make_step("cutoff", "sleep 1.5", 2);
                let mut second = make_step("cutoff_second", "sleep 1.5", 2);
                second.deps = vec![first.tag()];
                let mut third = make_step("cutoff_third", "sleep 1.5", 2);
                third.deps = vec![second.tag()];
                let mut pending = make_step("pending", "true", 2);
                pending.deps = vec![third.tag()];
                steps.extend([first, second, third, pending]);
            }
            let cfg = super::super::DagConfig {
                steps,
                ..Default::default()
            };
            let planned = cfg.steps.iter().map(|step| step.tag()).collect();
            let log = dir
                .path()
                .join(format!("cutoff-{prior_failure}-{cold_log}.log"));
            if !cold_log {
                std::fs::write(&log, "existing log\n").unwrap();
            }
            let allowance = if cold_log {
                2_000_000_000
            } else {
                5_000_000_000
            };
            let deadline = super::super::monotonic_now_ns().unwrap() + allowance;
            let result =
                super::super::run_lane_once(&cfg, 1, true, 0, None, &log, Some(deadline), false);
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
            if cold_log {
                // The original two-second/sleep-ten failing control: settling
                // the absent log spends the deadline, so neither selected node
                // may launch and both identities remain explicitly unattempted.
                assert!(result.outcomes.is_empty() && result.attempts.is_empty());
                assert_eq!(
                    result.skipped.iter().cloned().collect::<BTreeSet<_>>(),
                    planned
                );
            } else {
                assert!(
                    result
                        .outcomes
                        .iter()
                        .any(|outcome| outcome.tag == "classification.cutoff")
                );
            }
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
        for (name, allowance_ns, should_run) in [
            ("subsecond", 500_000_000, false),
            ("subsecond_after_settle", 1_050_000_000, false),
            ("positive", 2_500_000_000, true),
        ] {
            let sentinel = dir.path().join(format!("{name}.sentinel"));
            let command = format!(
                "printf ran > {}",
                validate_plan::shell_quote(&sentinel.to_string_lossy())
            );
            let cfg = super::super::DagConfig {
                steps: vec![make_step(name, &command, 1)],
                ..Default::default()
            };
            let log = dir.path().join(format!("{name}.log"));
            std::fs::write(&log, "existing log\n").unwrap();
            let started = super::super::monotonic_now_ns().unwrap();
            let result = super::super::run_lane_once(
                &cfg,
                1,
                true,
                0,
                None,
                &log,
                Some(started + allowance_ns),
                false,
            );
            assert_eq!(
                sentinel.exists(),
                should_run,
                "{name}: launch must follow the original shared allowance"
            );
            assert_eq!(result.run_timed_out, !should_run, "{name}");
            assert_eq!(result.complete, should_run, "{name}");
            if should_run {
                assert_eq!(result.outcomes.len(), 1);
                assert!(result.outcomes[0].ok);
            } else {
                assert!(result.outcomes.is_empty() && result.attempts.is_empty());
                assert_eq!(result.skipped, vec![format!("classification.{name}")]);
            }
        }
    }
}

#[cfg(test)]
#[path = "validate_nextest_fixture.rs"]
mod real_nextest_tests;

#[cfg(test)]
mod publication_tests {
    use super::*;

    fn refused(kind: Option<dagrun::TestResultsErrorKind>) -> NodeAttempt {
        let mut attempt = reported_attempt(&fixture_outcome("test.publication", 75), 1);
        attempt.test_results_error = Some("real import refusal; prose is not authority".into());
        attempt.test_results_error_kind = kind;
        attempt
    }

    #[test]
    fn typed_publication_inability_stays_unknown_but_never_hides_failed_evidence() {
        use dagrun::TestResultsErrorKind::InvalidReport;
        use dagrun::TestResultsErrorKind::Missing;
        use dagrun::TestResultsErrorKind::ReadIo;
        for kind in [Missing, ReadIo] {
            let attempt = refused(Some(kind));
            assert_eq!(
                attempt_classification(&attempt),
                NodeClassification::NoResult
            );
            for exit in [-15, 0, 1, 2, 101] {
                let mut command_failure = attempt.clone();
                command_failure.returncode = Some(exit);
                assert_eq!(
                    attempt_classification(&command_failure),
                    NodeClassification::ProductFailure
                );
            }
            for mask in 1..16 {
                let mut limited = attempt.clone();
                limited.timed_out = Some(mask & 1 != 0);
                limited.cpu_timed_out = Some(mask & 2 != 0);
                limited.oomed = Some(mask & 4 != 0);
                limited.oom_kills = Some(if mask & 8 != 0 { 1 } else { 0 });
                assert_eq!(
                    attempt_classification(&limited),
                    NodeClassification::ProductFailure
                );
            }
            let mut measured_failure = attempt.clone();
            measured_failure.test_results = Some(vec![
                dagrun::TestResult::new("real::failed".into(), false, 1).unwrap(),
            ]);
            assert_eq!(
                attempt_classification(&measured_failure),
                NodeClassification::ProductFailure
            );
            let outcome = fixture_outcome("test.publication", 75);
            let earlier = reported_attempt(&fixture_outcome("test.publication", 1), 1);
            let mut later = attempt.clone();
            later.attempt = 2;
            assert_eq!(
                node_classification(&outcome, &[earlier, later]),
                NodeClassification::ProductFailure
            );

            // A publication exit collected while cancellation drains is not a
            // completed child result or a measured child-budget breach. Keep
            // the scheduler's first cause separate from the import diagnostic.
            for (cut_by_run_budget, cause) in [
                (false, dagrun::model::ABORTED_BY_PEER_FAILURE_REASON),
                (true, dagrun::model::ABORTED_BY_RUN_BUDGET_REASON),
                (
                    false,
                    "ABORTED (required whole-run CPU accounting was lost; CPU budget exhaustion was not established)",
                ),
            ] {
                let mut aborted = StepOutcome::aborted_outcome(
                    "test.publication".into(),
                    0.25,
                    String::new(),
                    Some(super::super::NO_RESULT_EXIT_CODE),
                    None,
                    None,
                    cut_by_run_budget,
                );
                // The scheduler records accounting loss after constructing the
                // cancellation outcome; its public constructor takes the two
                // ordinary peer/run-budget causes directly.
                if cut_by_run_budget || cause == dagrun::model::ABORTED_BY_PEER_FAILURE_REASON {
                    assert_eq!(aborted.reason, cause);
                } else {
                    aborted.reason = cause.into();
                }
                aborted.test_results_error = Some("publication import unavailable".into());
                aborted.test_results_error_kind = Some(kind);
                let unknown = reported_attempt(&aborted, 1);
                assert!(unknown.reported && unknown.aborted);
                assert_eq!(unknown.ok, Some(false));
                assert_eq!(unknown.execution, AttemptExecution::Unknown);
                assert_eq!(unknown.returncode, Some(super::super::NO_RESULT_EXIT_CODE));
                assert_eq!(unknown.reason, cause);
                assert_eq!(unknown.test_results_error, aborted.test_results_error);
                assert_eq!(unknown.test_results_error_kind, Some(kind));
                assert_eq!(unknown.timed_out, Some(false));
                assert_eq!(unknown.cpu_timed_out, Some(false));
                assert_eq!(unknown.oomed, Some(false));
                assert_eq!(unknown.oom_kills, Some(0));
                assert!(!attempt_is_no_result(&unknown));
                assert_eq!(super::super::attempt_result(&unknown), None);
                assert_eq!(
                    super::super::completed_node_count(
                        std::slice::from_ref(&aborted),
                        std::slice::from_ref(&unknown),
                    ),
                    0
                );
                assert_eq!(attempt_classification(&unknown), NodeClassification::NoResult);

                let mut with_failed_row = unknown.clone();
                with_failed_row.test_results = Some(vec![
                    dagrun::TestResult::new("real::failed".into(), false, 1).unwrap(),
                ]);
                let mut latest = unknown.clone();
                latest.attempt = 2;
                for (name, attempts, wanted) in [
                    ("aborted_import", vec![unknown], NodeClassification::NoResult),
                    (
                        "retained_failed_row",
                        vec![with_failed_row],
                        NodeClassification::ProductFailure,
                    ),
                    (
                        "earlier_failure",
                        vec![
                            reported_attempt(&fixture_outcome("test.publication", 1), 1),
                            latest,
                        ],
                        NodeClassification::ProductFailure,
                    ),
                ] {
                    assert_eq!(node_classification(&aborted, &attempts), wanted, "{name}");
                    let gate = super::super::ledger_gate_with_attempts(&aborted, &attempts);
                    assert_eq!(gate["result"], wanted.result(), "{name}");
                    assert_eq!(gate["failure_class"], wanted.as_str(), "{name}");
                    assert_eq!(gate["aborted"], wanted == NodeClassification::NoResult);
                    assert_eq!(gate["raw_aborted"], true);
                    assert!(gate["raw_result"].is_null());
                    if wanted == NodeClassification::ProductFailure {
                        assert_eq!(gate["failure_origin"], "outer_gate");
                    }
                    let raw = gate["attempts"].as_array().unwrap().last().unwrap();
                    assert!(raw["result"].is_null());
                    assert_eq!(raw["aborted"], true);
                    for observation in [&gate, raw] {
                        assert_eq!(observation["reported"], true);
                        assert_eq!(observation["execution"], "unknown");
                        assert_eq!(observation["exit_code"], super::super::NO_RESULT_EXIT_CODE);
                        assert_eq!(observation["reason"], cause);
                        assert_eq!(
                            observation["test_results_error"],
                            "publication import unavailable"
                        );
                        assert_eq!(observation["test_results_error_kind"], kind.value());
                        for field in ["timed_out", "cpu_timed_out", "oomed"] {
                            assert_eq!(observation[field], false, "{name}: {field}");
                        }
                        assert_eq!(observation["oom_kills"], 0);
                    }
                    let typed: hermit_manifest_plan::ledger::GateHistoryRow =
                        serde_json::from_value(gate.clone()).unwrap();
                    let read_back = serde_json::to_value(typed).unwrap();
                    for (field, expected) in gate.as_object().unwrap() {
                        assert_eq!(&read_back[field], expected, "{name}: reader lost {field}");
                    }
                    println!("ABORTED_PUBLICATION_FIXTURE {name} {read_back}");
                }
            }
        }
        for kind in [None, Some(InvalidReport)] {
            let attempt = refused(kind);
            assert_eq!(
                attempt_classification(&attempt),
                NodeClassification::ProductFailure
            );
        }
        assert_eq!(
            dagrun::TestResultsErrorKind::from_value("future_kind"),
            None
        );
        let mut invalid_without_text = refused(Some(InvalidReport));
        invalid_without_text.test_results_error = None;
        assert_eq!(
            attempt_classification(&invalid_without_text),
            NodeClassification::ProductFailure
        );
        for kind in [ReadIo, InvalidReport] {
            let mut contradictory = refused(Some(kind));
            contradictory.returncode = Some(0);
            contradictory.test_results_error =
                Some("required structured test results were not written to fixture".into());
            assert_eq!(
                attempt_classification(&contradictory),
                NodeClassification::ProductFailure,
                "legacy prose must not override {kind:?}",
            );
        }
    }

    #[test]
    fn contradictory_success_never_excuses_a_required_result_refusal() {
        let mut attempt = refused(Some(dagrun::TestResultsErrorKind::Missing));
        attempt.ok = Some(true);
        attempt.returncode = Some(0);
        attempt.test_results_error =
            Some("required structured test results were not written to fixture".into());
        assert_eq!(
            attempt_classification(&attempt),
            NodeClassification::ProductFailure
        );
        attempt.test_results_error = None;
        attempt.test_results_error_kind = None;
        attempt.reason = "STRUCTURED TEST RESULTS REFUSED: required structured test results were not written to fixture".into();
        assert_eq!(
            attempt_classification(&attempt),
            NodeClassification::ProductFailure
        );
    }
}
