// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Assemble cumulative evidence from the exact selected plan and producer rows.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use hermit_manifest_plan::ledger::ConstructedPlanArtifact;
use hermit_manifest_plan::ledger::ConstructedValidationPlanV10;
use hermit_manifest_plan::ledger::HistoryRow;
use hermit_manifest_plan::ledger::ValidatePath;
use sha2::Digest;
use sha2::Sha256;

use super::AttemptExecution;
use super::LedgerCtx;
use super::NodeAttempt;
use super::Plan;
use super::StepOutcome;
use super::validate_cell_results::RetainedCellResults;
use super::validate_test_results::ExactTestTotals;
use super::validate_test_results::NodeTestResultsInput;
use super::validate_test_results::RetainedTestResults;
use super::validate_test_results::SelectedTestProducers;

// Retain cumulative evidence when the two existing selectors request parity.
pub const ENABLED: bool = true;

pub struct SelectedEvidence {
    dag_json: String,
    expected_path: PathBuf,
    expected_bytes: Vec<u8>,
    compatibility_selected: bool,
    path: ValidatePath,
}

pub struct PreparedEvidence {
    pub plan: ConstructedValidationPlanV10,
    reference: ConstructedPlanArtifact,
    bytes: Vec<u8>,
    producers: SelectedTestProducers,
}

pub struct RetainedEvidence {
    pub cells: RetainedCellResults,
    tests: RetainedTestResults,
    plan: ConstructedPlanArtifact,
    plan_bytes: Vec<u8>,
    cell_bytes: Vec<u8>,
    test_bytes: Vec<u8>,
}

impl SelectedEvidence {
    pub fn capture(root: &Path, plan: &Plan) -> Result<Self, String> {
        super::require_committed_scheduler_input(plan)?;
        let dag_json = plan
            .committed_selection
            .clone()
            .ok_or("cumulative evidence requires the exact committed selection")?;
        let path = serde_json::from_value(serde_json::Value::String(plan.profile.clone()))
            .map_err(|error| format!("unsupported cumulative evidence profile: {error}"))?;
        let expected_path = root.join("ci/expected-e2e-plan.json");
        let expected_bytes = std::fs::read(&expected_path)
            .map_err(|error| format!("cannot retain expected E2E plan: {error}"))?;
        Ok(Self {
            dag_json,
            expected_path,
            expected_bytes,
            compatibility_selected: plan.compat.is_some(),
            path,
        })
    }

    pub fn publish(
        self,
        parent: &Path,
        selected: &Plan,
        run_id: &str,
        hermit_sha: &str,
    ) -> Result<PreparedEvidence, String> {
        // Check both held source files again at the actual scheduling boundary.
        super::require_committed_scheduler_input(selected)?;
        if super::dag_to_json(&selected.cfg) != self.dag_json
            || selected.compat.is_some() != self.compatibility_selected
            || std::fs::read(&self.expected_path)
                .map_err(|error| format!("cannot recheck expected E2E plan: {error}"))?
                != self.expected_bytes
        {
            return Err("selected plan or source population changed before execution".into());
        }
        let plan = ConstructedValidationPlanV10 {
            schema: 1,
            run_id: run_id.into(),
            hermit_sha: hermit_sha.into(),
            path: self.path,
            compatibility_selected: self.compatibility_selected,
            dag_json: self.dag_json,
            expected_e2e_plan_json: String::from_utf8(self.expected_bytes)
                .map_err(|error| format!("expected E2E plan is not UTF-8: {error}"))?,
        };
        let cfg = plan.constructed_dag()?;
        plan.planned_cells()?;
        plan.planned_backend_parity_relations()?;
        let producers = SelectedTestProducers::from_constructed_plan_steps(
            &cfg.steps,
            plan.compatibility_selected,
        )?;
        let bytes = serde_json::to_vec(&plan).map_err(|error| error.to_string())?;
        let path = super::validate_artifacts::publish_run_artifact_noclobber(
            parent,
            run_id,
            "constructed-plan.json",
            &bytes,
            "constructed validation plan",
        )?;
        let reference = ConstructedPlanArtifact {
            path,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
        };
        Ok(PreparedEvidence {
            plan,
            reference,
            bytes,
            producers,
        })
    }
}

fn test_inputs(
    selected: &SelectedTestProducers,
    outcomes: &[StepOutcome],
    attempts: &[NodeAttempt],
    compat_prefix: Option<&str>,
) -> Result<(Vec<NodeTestResultsInput>, Option<dagrun::TestResults>), String> {
    let mut final_outcomes = BTreeMap::new();
    for outcome in outcomes {
        if final_outcomes
            .insert(outcome.tag.as_str(), outcome)
            .is_some()
        {
            return Err(format!("duplicate terminal outcome for {}", outcome.tag));
        }
    }
    let mut nodes = Vec::new();
    for tag in selected.nodes() {
        let outcome = final_outcomes
            .get(tag.as_str())
            .ok_or_else(|| format!("selected test producer {tag} has no terminal outcome"))?;
        let latest = super::terminal_attempt(outcome, attempts)
            .ok_or_else(|| format!("selected test producer {tag} has no attempt"))?;
        if !latest.reported || latest.execution != AttemptExecution::Completed || outcome.aborted {
            return Err(format!(
                "selected test producer {tag} did not complete with reported results"
            ));
        }
        let results = latest
            .test_results
            .clone()
            .ok_or_else(|| format!("selected test producer {tag} omitted per-test results"))?;
        if outcome.test_results.as_ref() != Some(&results) {
            return Err(format!(
                "selected test producer {tag} terminal results differ from its attempt"
            ));
        }
        let executed = outcome
            .executed_tests
            .ok_or_else(|| format!("selected test producer {tag} has no executed-test count"))?;
        let filtered = outcome
            .filtered_tests
            .ok_or_else(|| format!("selected test producer {tag} has no filtered-test count"))?;
        nodes.push(NodeTestResultsInput {
            node: tag.clone(),
            outer_attempt: u64::try_from(latest.attempt)
                .map_err(|_| format!("selected test producer {tag} has an invalid attempt"))?,
            test_results: super::validate_test_results::terminal_results(
                executed, filtered, results,
            )?,
        });
    }
    let compatibility = if selected.compatibility_selected() {
        Some(super::compat_test_results(
            outcomes,
            attempts,
            compat_prefix.ok_or("selected compatibility producer has no prefix")?,
        )?)
    } else {
        None
    };
    Ok((nodes, compatibility))
}

impl PreparedEvidence {
    pub fn retain(
        self,
        parent: &Path,
        result_root: &Path,
        ctx: &LedgerCtx,
        outcomes: &[StepOutcome],
        attempts: &[NodeAttempt],
        compat_prefix: Option<&str>,
    ) -> Result<RetainedEvidence, String> {
        if ctx.tree_dirty
            || ctx.commit != self.plan.hermit_sha
            || ctx.profile != self.plan.path.as_str()
        {
            return Err(
                "cumulative evidence requires the exact clean admitted source identity".into(),
            );
        }
        let exact = |value: Option<i64>, name: &str| -> Result<u64, String> {
            u64::try_from(value.ok_or_else(|| format!("{name} is unknown"))?)
                .map_err(|_| format!("{name} is negative"))
        };
        let expected = ExactTestTotals {
            executed_tests: exact(ctx.executed_tests, "executed_tests")?,
            passed_tests: exact(ctx.passed_tests, "passed_tests")?,
            filtered_tests: exact(ctx.filtered_tests, "filtered_tests")?,
        };
        let (nodes, compatibility) =
            test_inputs(&self.producers, outcomes, attempts, compat_prefix)?;
        let tests = super::validate_test_results::retain(
            parent,
            self.plan.path,
            &self.plan.run_id,
            &self.plan.hermit_sha,
            false,
            &self.producers,
            nodes,
            compatibility,
            expected,
        )?;
        let cells = super::validate_cell_results::retain_v10(parent, result_root, &self.plan)?;
        let cell_path = cells
            .evidence
            .get("artifact")
            .and_then(|a| a.get("path"))
            .and_then(serde_json::Value::as_str)
            .ok_or("retained cell evidence omitted its artifact path")?;
        let cell_bytes = std::fs::read(parent.join(cell_path))
            .map_err(|error| format!("cannot read retained cell artifact: {error}"))?;
        let test_bytes = std::fs::read(parent.join(&tests.evidence.artifact.path))
            .map_err(|error| format!("cannot read retained test artifact: {error}"))?;
        let retained = RetainedEvidence {
            cells,
            tests,
            plan: self.reference,
            plan_bytes: self.bytes,
            cell_bytes,
            test_bytes,
        };
        let mut row = serde_json::json!({ "schema_version":10, "run_id":self.plan.run_id,
            "commit":ctx.commit, "profile":ctx.profile, "tree_dirty":ctx.tree_dirty,
            "executed_tests":ctx.executed_tests, "passed_tests":ctx.passed_tests,
            "filtered_tests":ctx.filtered_tests });
        retained.add_to_record(&mut row)?;
        retained.verify_record(&serde_json::from_value(row).map_err(|error| error.to_string())?)?;
        Ok(retained)
    }
}

impl RetainedEvidence {
    pub fn add_to_record(&self, record: &mut serde_json::Value) -> Result<(), String> {
        if self.cells.schema_version != 10 || self.tests.schema_version != 9 {
            return Err("cumulative evidence components carry unexpected versions".into());
        }
        record["schema_version"] = serde_json::Value::from(10);
        record["constructed_plan"] =
            serde_json::to_value(&self.plan).map_err(|error| error.to_string())?;
        record["cell_results"] = self.cells.evidence.clone();
        record["test_results"] =
            serde_json::to_value(&self.tests.evidence).map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn verify_record(&self, row: &HistoryRow) -> Result<(), String> {
        row.verify_schema10_artifact_bytes(&self.plan_bytes, &self.cell_bytes, &self.test_bytes)?
            .ok_or("cumulative evidence lost its explicit schema dispatch")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_non_test_group_producers_require_real_terminal_rows() {
        let cfg = super::super::dag_from_json(
            &serde_json::json!({"steps":[{
                "group":"check", "job":"fixture", "cmd":"true",
                "result_manifests":[{"kind":"structured-test-results","schema":2,
                    "path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"check.fixture"}]
            }]})
            .to_string(),
        )
        .unwrap();
        let selected =
            SelectedTestProducers::from_constructed_plan_steps(&cfg.steps, false).unwrap();
        let outcome = StepOutcome {
            tag: "check.fixture".into(),
            ok: false,
            duration_s: 0.0,
            summary: String::new(),
            executed_tests: Some(1),
            filtered_tests: Some(2),
            test_results: Some(vec![
                dagrun::TestResult::new("failed-case".into(), false, 2).unwrap(),
            ]),
            test_results_error: None,
            test_results_error_kind: None,
            returncode: Some(1),
            oomed: false,
            oom_kills: 0,
            timed_out: false,
            cpu_timed_out: false,
            reason: "fixture failed".into(),
            aborted: false,
        };
        let attempt = super::super::reported_attempt(&outcome, 1);
        let (nodes, compatibility) = test_inputs(
            &selected,
            std::slice::from_ref(&outcome),
            std::slice::from_ref(&attempt),
            None,
        )
        .unwrap();
        assert!(compatibility.is_none());
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node, "check.fixture");
        assert_eq!(nodes[0].test_results.executed_tests, 1);
        assert_eq!(nodes[0].test_results.filtered_tests, 2);
        assert_eq!(
            nodes[0].test_results.results.as_ref().unwrap()[0].attempts,
            2
        );
        assert!(!nodes[0].test_results.results.as_ref().unwrap()[0].passed);
        for mutation in [
            "absent",
            "unreported",
            "unknown",
            "different-results",
            "count-only",
            "duplicate",
        ] {
            let mut outcomes = vec![outcome.clone()];
            let mut attempts = vec![attempt.clone()];
            match mutation {
                "absent" => outcomes.clear(),
                "unreported" => attempts[0].reported = false,
                "unknown" => attempts[0].execution = AttemptExecution::Unknown,
                "different-results" => attempts[0].test_results.as_mut().unwrap()[0].passed = true,
                "count-only" => {
                    outcomes[0].test_results = None;
                    attempts[0].test_results = None;
                }
                "duplicate" => outcomes.push(outcome.clone()),
                _ => unreachable!(),
            }
            assert!(
                test_inputs(&selected, &outcomes, &attempts, None).is_err(),
                "{mutation} admitted"
            );
        }
    }

    #[test]
    fn plan_publication_binds_compatibility_and_rechecks_source_before_execution() {
        for profile in ["full", "cell-requalification"] {
            let root = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(root.path().join("ci/dag")).unwrap();
            let cfg = super::super::dag_from_json(
                &serde_json::json!({"steps":[{
                    "group":"check", "job":"fixture", "cmd":"true",
                    "result_manifests":[{"kind":"structured-test-results","schema":2,
                        "path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"check.fixture"}]
                }]})
                .to_string(),
            )
            .unwrap();
            let dag = super::super::dag_to_json(&cfg);
            let path = root.path().join("ci/dag/validate.json");
            std::fs::write(&path, &dag).unwrap();
            let expected = root.path().join("ci/expected-e2e-plan.json");
            std::fs::write(&expected, "{\"schema\":1,\"cells\":[]}").unwrap();
            let mut plan = super::super::finish_committed_selection(
                Plan {
                    cfg,
                    profile: profile.into(),
                    ..Plan::default()
                },
                path,
                dag.into_bytes(),
            );
            let selected = SelectedEvidence::capture(root.path(), &plan).unwrap();
            let prepared = selected
                .publish(root.path(), &plan, "synthetic-plan", &"a".repeat(40))
                .unwrap();
            assert!(!prepared.plan.compatibility_selected);
            assert_eq!(prepared.plan.path.as_str(), profile);
            assert_eq!(
                std::fs::read(root.path().join(&prepared.reference.path)).unwrap(),
                prepared.bytes
            );
            let changed = SelectedEvidence::capture(root.path(), &plan).unwrap();
            std::fs::write(&expected, "{\"schema\":1,\"cells\":[]}\n").unwrap();
            assert!(
                changed
                    .publish(root.path(), &plan, "changed-plan", &"a".repeat(40))
                    .is_err()
            );
            assert!(
                !root
                    .path()
                    .join("ignored/validate/artifacts/changed-plan/constructed-plan.json")
                    .exists()
            );
            plan.profile = "cellrequalification".into();
            let unknown = SelectedEvidence::capture(root.path(), &plan)
                .err()
                .expect("unknown profile must remain refused");
            assert!(unknown.contains("unsupported cumulative evidence profile"));
        }
    }

    #[test]
    fn requalification_captures_and_publishes_the_committed_owner_without_full_authority() {
        use super::super::DagManifest;
        use super::super::build_plan;
        use super::super::dag_to_json;
        use super::super::parse_argv;

        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let args = parse_argv(&[
            "--requalify-cell".into(),
            "backend-parity-c/pid-probe".into(),
            "verify".into(),
            "liteinst".into(),
            "--no-label-pr".into(),
        ])
        .unwrap();
        let plan = build_plan(root, &args, scratch.path()).unwrap();
        assert_eq!(plan.profile, "cell-requalification");
        assert_eq!(plan.selection_mode, "targeted");
        assert!(!plan.suite_complete);
        assert!(plan.second.is_none());

        let requested = DagManifest {
            lane: "portable".into(),
            category: "backend-parity-c".into(),
            test: Some("backend-parity-c/pid-probe".into()),
            mode: Some("verify".into()),
            backend: Some("liteinst".into()),
        };
        let committed = super::super::validate_plan::validation_config(root).unwrap();
        let lane =
            dagrun::select_steps_by_labels(&committed, std::slice::from_ref(&requested.lane))
                .unwrap();
        let owner = dagrun::result_manifest_owner(&lane.steps, &requested).unwrap();
        let selected = dagrun::select_steps_by_tags(&lane, &[owner.tag()], false).unwrap();
        // Exact bytes cover resource bounds, command argv and dependency order.
        assert_eq!(dag_to_json(&plan.cfg), dag_to_json(&selected));
        let prepared = SelectedEvidence::capture(root, &plan)
            .unwrap()
            .publish(
                scratch.path(),
                &plan,
                "requalification-pid-probe",
                &"a".repeat(40),
            )
            .unwrap();
        assert_eq!(prepared.plan.path.as_str(), plan.profile);
        assert_eq!(prepared.plan.dag_json, dag_to_json(&selected));
        assert_eq!(
            prepared.plan.expected_e2e_plan_json.as_bytes(),
            std::fs::read(root.join("ci/expected-e2e-plan.json")).unwrap()
        );
        let mut expected: Vec<hermit_manifest_plan::ledger::CellIdentity> =
            serde_json::from_value(serde_json::to_value(&plan.cell_evidence_expected).unwrap())
                .unwrap();
        expected.sort();
        assert_eq!(prepared.plan.planned_cells().unwrap(), expected);
        assert!(
            prepared
                .plan
                .planned_backend_parity_relations()
                .unwrap()
                .iter()
                .any(|relation| {
                    relation.candidate.test == "backend-parity-c/pid-probe"
                        && relation.candidate.mode == "verify"
                        && relation.candidate.backend == "liteinst"
                        && relation.reference_backend == "ptrace"
                })
        );
        assert_eq!(
            std::fs::read(scratch.path().join(&prepared.reference.path)).unwrap(),
            prepared.bytes
        );
        let refusal = super::super::validate_receipt::eligible(
            0,
            0,
            true,
            true,
            false,
            prepared.plan.path.as_str(),
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "profile is cell-requalification, not the full suite"
        );
        // Preserve the existing ptrace selection and mutation controls as well.
        super::super::requalification_plan_bracket(root).unwrap();
    }
}
