/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use super::*;

fn retain_fixture(name: &str, row: &HistoryRow, plan: &[u8], cells: &[u8], tests: &[u8]) {
    use std::io::Write;
    let Some(root) = std::env::var_os("HERMIT_SCHEMA10_FIXTURE_OUTPUT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    assert!(
        root.is_absolute(),
        "fixture artifact destination must be absolute"
    );
    std::fs::create_dir_all(&root).unwrap();
    let row_bytes = serde_json::to_vec(row).unwrap();
    for (suffix, bytes) in [
        ("row.json", row_bytes.as_slice()),
        ("plan.json", plan),
        ("cells.jsonl", cells),
        ("tests.jsonl", tests),
    ] {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join(format!("{name}-{suffix}")))
            .unwrap();
        output.write_all(bytes).unwrap();
    }
}

// A cell of a plan retained from before the backend-parity-c bucket was
// folded into c-programs (https://github.com/rrnewton/hermit/issues/3301,
// slice S6): the reader must keep verifying such plans with their original
// identities and selector, so this fixture deliberately keeps the retired ones.
fn identity() -> CellIdentity {
    CellIdentity {
        lane: "portable".into(),
        category: "backend-parity-c".into(),
        test: "backend-parity-c/fixture".into(),
        mode: "verify".into(),
        backend: "kvm".into(),
    }
}

// These are synthetic parser controls, not guest measurements. The existing
// typed parity fixture supplies complete strict reports and comparison fields.
fn attempt(backend: &str, index: &str, report: &VerificationReport) -> ParityAttempt {
    let raw = serde_json::to_string(report).unwrap();
    ParityAttempt(AttemptResult {
        index: index.into(),
        outcome: "PASS".into(),
        error_kind: None,
        status: Some(0),
        signal: None,
        timed_out: false,
        duration_ms: 1,
        cpu_usage_usec: Some(1),
        observation_sha256: None,
        argv: vec![
            "/synthetic/fixture/hermit".into(),
            "--backend".into(),
            backend.into(),
            "run".into(),
            "--strict".into(),
            "--verify".into(),
            "--verify-strict".into(),
            "--".into(),
            "/synthetic/fixture/guest".into(),
        ],
        guest_argv: vec!["/synthetic/fixture/guest".into()],
        env: BTreeMap::new(),
        cwd: "/synthetic/fixture/work".into(),
        shell_command: "/synthetic/fixture/hermit run a synthetic fixture".into(),
        stdout: "fixture output".into(),
        stderr: String::new(),
        verification_report_sha256: Some(hex_digest(raw.as_bytes())),
        verification_report: Some(raw),
        runtime: report.runtime.clone(),
        first_divergent_scheduler_turn: report.first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds: report.first_divergent_virtual_nanoseconds,
        first_divergent_record: report.first_divergent_record,
        first_divergent_syscall: report.first_divergent_syscall,
        first_divergent_left_message: report.first_divergent_left_message.clone(),
        first_divergent_right_message: report.first_divergent_right_message.clone(),
        sabre_path_evidence: None,
        sabre_path_evidence_sha256: None,
        reason: None,
    })
}

fn completed(number: u64, verdict: BackendParityVerdict) -> BackendParityCellAttempt {
    let report = crate::backend_parity::tests::report(verdict);
    BackendParityCellAttempt::Completed {
        attempt: number,
        candidate_attempt: attempt("kvm", "1", &report.candidate.verification),
        reference_attempt: attempt("ptrace", "parity-reference", &report.reference.verification),
        report: Box::new(report),
    }
}

fn parity(attempts: Vec<BackendParityCellAttempt>) -> CellBackendParity {
    CellBackendParity {
        reference_backend: "ptrace".into(),
        record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
        attempts,
    }
}

fn fixture(parity: CellBackendParity) -> (HistoryRow, Vec<u8>, Vec<u8>, Vec<u8>) {
    fixture_with_path(parity, ValidatePath::Full)
}

fn fixture_with_path(
    parity: CellBackendParity,
    path: ValidatePath,
) -> (HistoryRow, Vec<u8>, Vec<u8>, Vec<u8>) {
    let id = identity();
    let tag = "e2e.manifest_backend_parity_c_on_host";
    let cfg = dag_from_json(&serde_json::json!({ "steps": [{
        "group": "e2e", "job": "manifest_backend_parity_c_on_host",
        "cmd": crate::backend_parity_policy::HOSTED_PARITY_COMMAND,
        "manifest": {"lane":"portable", "category":"backend-parity-c"},
        "result_manifests": [
            {"lane":id.lane, "category":id.category, "test":id.test, "mode":id.mode, "backend":id.backend},
            {"kind":"structured-test-results", "schema":2, "path_env":"DAGRUN_TEST_COUNTS_PATH", "owner":tag}
        ]
    }] }).to_string()).unwrap();
    let run_id = "fixture-v10".to_string();
    let hermit_sha = "a".repeat(40);
    let plan = ConstructedValidationPlanV10 {
        schema: 1,
        run_id: run_id.clone(),
        hermit_sha: hermit_sha.clone(),
        path,
        compatibility_selected: false,
        dag_json: dag_to_json(&cfg),
        expected_e2e_plan_json: serde_json::json!({"schema":1,"cells":[id]}).to_string(),
    };
    let plan_bytes = serde_json::to_vec(&plan).unwrap();
    let cell = CellArtifactResultV10 {
        cpu_observation_history: None,
        lane: id.lane.clone(),
        category: id.category.clone(),
        test: id.test.clone(),
        mode: id.mode.clone(),
        backend: id.backend.clone(),
        cell_verdict: parity.candidate_verdict(&id).unwrap(),
        selected_attempt: Some(parity.candidate_attempt_number(&id).unwrap()),
        backend_parity: RequiredNullable::Value(parity),
    };
    let mut cell_row = serde_json::to_value(&cell).unwrap();
    cell_row["run_id"] = Value::String(run_id.clone());
    cell_row["hermit_sha"] = Value::String(hermit_sha.clone());
    cell_row["source_tree_dirty"] = Value::Bool(false);
    let mut cell_bytes = serde_json::to_vec(&cell_row).unwrap();
    cell_bytes.push(b'\n');
    let selected = vec![id.clone()];
    let population = serde_json::to_vec(&serde_json::to_value(&selected).unwrap()).unwrap();
    let cells = CellResultsEvidenceV10 {
        binding_contract: CellBindingContract::SelectedAttemptV1,
        path,
        run_id: run_id.clone(),
        hermit_sha: hermit_sha.clone(),
        source_tree_dirty: false,
        selected_count: 1,
        recorded_count: 1,
        population_sha256: hex_digest(&population),
        artifact: CellResultsArtifact {
            path: format!("ignored/validate/artifacts/{run_id}/cell-results.jsonl"),
            sha256: hex_digest(&cell_bytes),
            row_count: 1,
        },
        selected,
        selected_backend_parity: vec![BackendParityRelation::ptrace(id)],
        cells: vec![cell.summary(&run_id, &hermit_sha).unwrap()],
    };
    let test_row = TestResultArtifactRow {
        run_id: run_id.clone(),
        hermit_sha: hermit_sha.clone(),
        path,
        producer: TestResultProducer::Node {
            node: tag.into(),
            outer_attempt: 1,
        },
        id: "synthetic fixture".into(),
        result: TestResultVerdict::Pass,
        attempts: 1,
    };
    let mut test_bytes = serde_json::to_vec(&test_row).unwrap();
    test_bytes.push(b'\n');
    let selected = TestResultsSelectedPopulation {
        nodes: vec![tag.into()],
        compatibility: false,
    };
    let totals = TestResultTotals {
        executed_tests: 1,
        passed_tests: 1,
        failed_tests: 0,
        filtered_tests: 0,
        diagnostic_failed_tests: 0,
    };
    let tests = TestResultsEvidenceV9 {
        path,
        run_id: run_id.clone(),
        hermit_sha: hermit_sha.clone(),
        source_tree_dirty: false,
        selected_count: 1,
        recorded_count: 1,
        population_sha256: hex_digest(&serde_json::to_vec(&selected).unwrap()),
        selected,
        nodes: vec![NodeTestResultSummary {
            node: tag.into(),
            outer_attempt: 1,
            totals,
            row_count: 1,
        }],
        compatibility: None,
        totals,
        artifact: TestResultsArtifact {
            path: format!("ignored/validate/artifacts/{run_id}/test-results.jsonl"),
            sha256: hex_digest(&test_bytes),
            row_count: 1,
        },
    };
    let row = serde_json::from_value(serde_json::json!({"schema_version":10,"run_id":run_id,"commit":hermit_sha,"profile":path.as_str(),"tree_dirty":false,
        "executed_tests":1,"passed_tests":1,"filtered_tests":0,"cell_results":cells,"test_results":tests,
        "constructed_plan":ConstructedPlanArtifact {path:format!("ignored/validate/artifacts/{run_id}/constructed-plan.json"),sha256:hex_digest(&plan_bytes),bytes:plan_bytes.len() as u64}
    })).unwrap();
    (row, plan_bytes, cell_bytes, test_bytes)
}

#[test]
fn exact_artifacts_derive_compact_summaries_and_refuse_independent_mutations() {
    let (row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert!(verified.full_backend_parity && verified.full_test_results);
    assert_eq!(
        verified.cell_results.binding_contract,
        CellBindingContract::SelectedAttemptV1
    );
    assert_eq!(verified.cell_results.bound_attempts().unwrap().len(), 1);
    retain_fixture("matched", &row, &plan, &cells, &tests);
    assert_eq!(verified.observations.len(), 3);
    assert!(verified.missing_cells.is_empty() && verified.missing_backend_parity.is_empty());
    for role in ["candidate_attempt", "reference_attempt"] {
        for flag in [
            "--strict",
            "--no-rcb-time",
            "--no-detlog-io-buffers",
            "--no-virtualize-cpuid",
            "--no-virtualize-metadata",
            "--no-virtualize-time",
            "--no-sequentialize-threads",
            "--no-deterministic-io",
            "--no-virtualize-cpuid=true",
            "--no-unknown-future-policy",
        ] {
            let mut cell: Value = serde_json::from_slice(&cells).unwrap();
            let argv = cell["backend_parity"]["attempts"][0][role]["argv"]
                .as_array_mut()
                .unwrap();
            if flag == "--strict" {
                let before = argv.len();
                argv.retain(|arg| arg.as_str() != Some(flag));
                assert_eq!(before - argv.len(), 1);
            } else {
                let separator = argv
                    .iter()
                    .position(|arg| arg.as_str() == Some("--"))
                    .unwrap();
                argv.insert(separator, Value::String(flag.into()));
            }
            let mut changed_cells = serde_json::to_vec(&cell).unwrap();
            changed_cells.push(b'\n');
            let mut changed_row = serde_json::to_value(&row).unwrap();
            changed_row["cell_results"]["artifact"]["sha256"] = hex_digest(&changed_cells).into();
            let changed_row: HistoryRow = serde_json::from_value(changed_row).unwrap();
            assert!(
                changed_row
                    .verify_schema10_artifact_bytes(&plan, &changed_cells, &tests)
                    .unwrap_err()
                    .contains("strict verify role"),
                "{role} {flag}"
            );
        }
    }
    // Ordinary-only selection remains valid ordinary evidence, but it is not
    // a measurement of parity over an empty denominator.
    let mut ordinary_plan: ConstructedValidationPlanV10 = serde_json::from_slice(&plan).unwrap();
    let mut cfg = ordinary_plan.constructed_dag().unwrap();
    cfg.steps[0].cmd = crate::backend_parity_policy::HOSTED_ORDINARY_COMMAND.into();
    ordinary_plan.dag_json = dag_to_json(&cfg);
    let ordinary_plan = serde_json::to_vec(&ordinary_plan).unwrap();
    let mut ordinary_cells: Value = serde_json::from_slice(&cells).unwrap();
    ordinary_cells["backend_parity"] = Value::Null;
    let mut ordinary_cells = serde_json::to_vec(&ordinary_cells).unwrap();
    ordinary_cells.push(b'\n');
    let mut ordinary_row = serde_json::to_value(&row).unwrap();
    ordinary_row["cell_results"]["selected_backend_parity"] = serde_json::json!([]);
    ordinary_row["cell_results"]["cells"][0]["backend_parity"] = Value::Null;
    ordinary_row["cell_results"]["artifact"]["sha256"] = hex_digest(&ordinary_cells).into();
    ordinary_row["constructed_plan"]["sha256"] = hex_digest(&ordinary_plan).into();
    ordinary_row["constructed_plan"]["bytes"] = (ordinary_plan.len() as u64).into();
    let ordinary_row: HistoryRow = serde_json::from_value(ordinary_row).unwrap();
    let ordinary = ordinary_row
        .verify_schema10_artifact_bytes(&ordinary_plan, &ordinary_cells, &tests)
        .unwrap()
        .unwrap();
    assert!(ordinary.cell_results.selected_backend_parity.is_empty());
    assert!(!ordinary.full_backend_parity);
    assert!(
        ordinary.full_test_results
            && ordinary.missing_cells.is_empty()
            && ordinary.missing_backend_parity.is_empty()
    );
    assert_eq!(ordinary.observations.len(), 1);
    assert_eq!(
        ordinary.observations[0].relation,
        ComparisonRelationV10::Ordinary
    );
    assert_eq!(
        ordinary.observations[0].verdict,
        ComparisonObservationVerdictV10::Matched
    );
    retain_fixture(
        "ordinary-only",
        &ordinary_row,
        &ordinary_plan,
        &ordinary_cells,
        &tests,
    );
    let compact = serde_json::to_string(&verified.cell_results).unwrap();
    assert!(!compact.contains("/synthetic/fixture"));
    assert!(
        String::from_utf8(cells.clone())
            .unwrap()
            .contains("/synthetic/fixture")
    );
    for field in [
        "candidate_verification_report_sha256",
        "reference_verification_report_sha256",
    ] {
        let mut value = serde_json::to_value(&row).unwrap();
        value["cell_results"]["cells"][0]["backend_parity"]["attempts"][0][field] =
            Value::String("f".repeat(64));
        let changed: HistoryRow = serde_json::from_value(value).unwrap();
        assert!(
            changed
                .verify_schema10_artifact_bytes(&plan, &cells, &tests)
                .unwrap_err()
                .contains("compact ledger summary")
        );
    }
    let mut changed = plan.clone();
    changed.push(b' ');
    assert!(
        row.verify_schema10_artifact_bytes(&changed, &cells, &tests)
            .is_err()
    );
    let mut value = serde_json::to_value(&row).unwrap();
    value["cell_results"]["cells"][0]["backend_parity"] = Value::Null;
    assert!(
        serde_json::from_value::<HistoryRow>(value)
            .unwrap()
            .verify_schema10_artifact_bytes(&plan, &cells, &tests)
            .is_err()
    );
    let mut missing = row.clone();
    let mut evidence = missing.schema10_cell_results().unwrap().unwrap();
    evidence.cells.clear();
    evidence.recorded_count = 0;
    evidence.artifact.row_count = 0;
    evidence.artifact.sha256 = hex_digest(b"");
    missing.cell_results = Some(CellResultsValue::Other(
        serde_json::to_value(evidence).unwrap(),
    ));
    let verified = missing
        .verify_schema10_artifact_bytes(&plan, b"", &tests)
        .unwrap()
        .unwrap();
    assert_eq!(verified.missing_cells, [identity()]);
    assert_eq!(
        verified.missing_backend_parity,
        [BackendParityRelation::ptrace(identity())]
    );
    assert!(!verified.full_backend_parity);
}

#[test]
fn reference_refusal_and_cross_divergence_never_change_the_candidate_verdict() {
    let complete = completed(1, BackendParityVerdict::Matched);
    let mut reference = complete.reference_attempt().unwrap().clone();
    reference.0.outcome = "ERROR".into();
    reference.0.status = Some(7);
    reference.0.error_kind = Some("incomplete-verification-evidence".into());
    reference.0.reason = Some("/synthetic/fixture/reference exited before JSON".into());
    reference.0.verification_report = None;
    reference.0.verification_report_sha256 = None;
    let unavailable = BackendParityCellAttempt::UnavailableWithReason {
        attempt: 1,
        candidate_attempt: complete.candidate_attempt().clone(),
        reference_attempt: RequiredNullable::Value(reference.clone()),
        reason: "/synthetic/fixture/reference did not finish".into(),
    };
    let (row, plan, cells, tests) = fixture(parity(vec![unavailable]));
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert!(!verified.full_backend_parity);
    assert_eq!(
        verified.observations[0].verdict,
        ComparisonObservationVerdictV10::Matched
    );
    assert!(matches!(
        verified.observations[1].verdict,
        ComparisonObservationVerdictV10::UnavailableWithReason { .. }
    ));
    reference.0.outcome = "PASS".into();
    assert!(
        reference
            .ordinary_verdict("ptrace", "parity-reference")
            .is_err()
    );
    reference.0.outcome = "ERROR".into();
    reference.0.verification_report_sha256 = Some("a".repeat(64));
    assert!(
        reference
            .ordinary_verdict("ptrace", "parity-reference")
            .is_err()
    );
    let mut divergent_report: VerificationReport = serde_json::from_str(
        complete
            .reference_attempt()
            .unwrap()
            .0
            .verification_report
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    divergent_report.verdict = Verdict::Diverged;
    divergent_report.verified = false;
    divergent_report.bitwise_parity = false;
    divergent_report.first_divergent_record = Some(1);
    let mut divergent_reference = attempt("ptrace", "parity-reference", &divergent_report);
    divergent_reference.0.outcome = "FAIL".into();
    divergent_reference.0.status = Some(1);
    divergent_reference.0.reason = Some("reference strict verification diverged".into());
    let (row, plan, cells, tests) = fixture(parity(vec![
        BackendParityCellAttempt::UnavailableWithReason {
            attempt: 1,
            candidate_attempt: complete.candidate_attempt().clone(),
            reference_attempt: RequiredNullable::Value(divergent_reference),
            reason: "reference strict verification diverged before cross comparison".into(),
        },
    ]));
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert_eq!(
        verified.observations[0].verdict,
        ComparisonObservationVerdictV10::Matched
    );
    assert_eq!(
        verified.observations[1].verdict,
        ComparisonObservationVerdictV10::Diverged
    );
    assert!(matches!(
        verified.observations[2].verdict,
        ComparisonObservationVerdictV10::UnavailableWithReason { .. }
    ));
    assert!(!verified.full_backend_parity);
    assert!(matches!(
        verified.cell_results.cells[0].cell_verdict,
        CellVerdict::ComparedAndMatched { .. }
    ));
    retain_fixture("reference-diverged", &row, &plan, &cells, &tests);
    // Rehash every affected container: the parser must reject the actual
    // contradictory operand, not merely a stale enclosing digest.
    for mutation in [
        "mismatched-output",
        "invalid-digest",
        "missing-disposition",
        "guest-disposition",
    ] {
        let mut cell: Value = serde_json::from_slice(&cells).unwrap();
        let candidate = &mut cell["backend_parity"]["attempts"][0]["candidate_attempt"];
        let mut report: Value =
            serde_json::from_str(candidate["verification_report"].as_str().unwrap()).unwrap();
        match mutation {
            "mismatched-output" => {
                report["compared_outputs"]["right"]["stdout_sha256"] = Value::String("f".repeat(64))
            }
            "invalid-digest" => {
                report["compared_outputs"]["left"]["stdout_sha256"] =
                    Value::String("invalid".into());
                report["compared_outputs"]["right"]["stdout_sha256"] =
                    Value::String("invalid".into());
            }
            "missing-disposition" => {
                report["compared_outputs"]["left"]["exit_code"] = Value::Null;
                report["compared_outputs"]["right"]["exit_code"] = Value::Null;
            }
            "guest-disposition" => report["guest_exit_code"] = Value::from(9),
            _ => unreachable!(),
        }
        let raw = serde_json::to_string(&report).unwrap();
        let digest = hex_digest(raw.as_bytes());
        candidate["verification_report"] = Value::String(raw);
        candidate["verification_report_sha256"] = Value::String(digest.clone());
        let mut changed_cells = serde_json::to_vec(&cell).unwrap();
        changed_cells.push(b'\n');
        let mut changed_row = serde_json::to_value(&row).unwrap();
        changed_row["cell_results"]["cells"][0]["backend_parity"]["attempts"][0]["candidate_verification_report_sha256"] =
            Value::String(digest);
        changed_row["cell_results"]["artifact"]["sha256"] =
            Value::String(hex_digest(&changed_cells));
        let changed_row: HistoryRow = serde_json::from_value(changed_row).unwrap();
        assert!(
            changed_row
                .verify_schema10_artifact_bytes(&plan, &changed_cells, &tests)
                .is_err(),
            "{mutation} became an ordinary match while cross comparison was unavailable"
        );
    }

    let (row, plan, cells, tests) = fixture(parity(vec![
        completed(1, BackendParityVerdict::Diverged),
        completed(2, BackendParityVerdict::Matched),
    ]));
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert!(!verified.full_backend_parity);
    assert_eq!(verified.observations.len(), 6);
    retain_fixture("cross-diverged-then-matched", &row, &plan, &cells, &tests);
    assert_eq!(
        verified.observations[2].verdict,
        ComparisonObservationVerdictV10::Diverged
    );
    assert_eq!(
        verified.observations[5].verdict,
        ComparisonObservationVerdictV10::Matched
    );
    assert!(matches!(
        verified.cell_results.cells[0].cell_verdict,
        CellVerdict::ComparedAndMatched { .. }
    ));
}

#[test]
fn parity_attempt_decoder_requires_every_nullable_key_and_binds_raw_reports() {
    let diversity = br#"{"duration_ms":null,"diversity":{"normalized_entropy":0.5},"env":{"duration_ms":"guest value"},"attempts":[{"duration_ms":1,"env":{"duration_ms":"also a guest value"}}]}"#;
    assert_eq!(
        read_schema10_source_result(diversity).unwrap(),
        serde_json::from_slice::<Value>(diversity).unwrap()
    );
    let completed = completed(1, BackendParityVerdict::Matched);
    let candidate = completed.candidate_attempt();
    let value = serde_json::to_value(candidate).unwrap();
    for key in value.as_object().unwrap().keys() {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ParityAttempt>(missing).is_err(),
            "missing {key} was accepted"
        );
    }
    let mut largest_exact = value.clone();
    largest_exact["duration_ms"] = Value::from(u64::MAX);
    assert_eq!(
        read_schema10_source_result(&serde_json::to_vec(&largest_exact).unwrap()).unwrap(),
        largest_exact
    );
    assert_eq!(
        serde_json::from_value::<ParityAttempt>(largest_exact)
            .unwrap()
            .0
            .duration_ms,
        u128::from(u64::MAX)
    );
    let text = serde_json::to_string(&value).unwrap();
    for duration in [
        "18446744073709551616",
        "340282366920938463463374607431768211455",
        "1.0",
        "1e0",
    ] {
        let changed = text.replace(
            "\"duration_ms\":1,",
            &format!("\"duration_ms\":{duration},"),
        );
        assert_ne!(changed, text);
        assert!(
            serde_json::from_str::<ParityAttempt>(&changed).is_err(),
            "duration {duration} was rounded or coerced"
        );
        assert!(
            read_schema10_source_result(changed.as_bytes()).is_err(),
            "original duration {duration} was rounded before exact decoding"
        );
    }
    let historical = text.replace(
        "\"duration_ms\":1,",
        "\"duration_ms\":340282366920938463463374607431768211455,",
    );
    assert_eq!(
        serde_json::from_str::<AttemptResult>(&historical)
            .unwrap()
            .duration_ms,
        u128::MAX
    );
    let mut extra = value.clone();
    extra["unknown"] = Value::Null;
    assert!(serde_json::from_value::<ParityAttempt>(extra).is_err());
    let text = serde_json::to_string(&value).unwrap();
    let duplicate = text.replacen('{', "{\"status\":0,", 1);
    assert!(serde_json::from_str::<ParityAttempt>(&duplicate).is_err());
    let mut changed = candidate.clone();
    changed.0.verification_report.as_mut().unwrap().push(' ');
    assert!(
        changed
            .ordinary_verdict("kvm", "1")
            .unwrap_err()
            .contains("SHA256")
    );
    let mut changed = candidate.clone();
    changed.0.argv.insert(1, "--backend=ptrace".into());
    assert!(changed.ordinary_verdict("kvm", "1").is_err());
    let mut changed = completed.clone();
    if let BackendParityCellAttempt::Completed { report, .. } = &mut changed {
        report.comparison.inputs = None;
    }
    assert!(changed.verdicts(&identity()).is_err());
}

#[test]
fn retained_plan_refuses_changed_selection_policy_and_test_denominators() {
    generated_plan_populations_preserve_command_policy();
    let (row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let original: ConstructedValidationPlanV10 = serde_json::from_slice(&plan).unwrap();
    for mutation in [
        "command",
        "owned-population",
        "compatibility",
        "producer",
        "expected-duplicate",
    ] {
        let mut plan = original.clone();
        let mut cfg = plan.constructed_dag().unwrap();
        match mutation {
            "command" => {
                cfg.steps[0].cmd = cfg.steps[0].cmd.replace("--parity-reference ptrace", "")
            }
            "owned-population" => cfg.steps[0]
                .result_manifests
                .as_mut()
                .unwrap()
                .retain(|item| {
                    matches!(
                        item,
                        dagrun::model::ResultManifest::StructuredTestResults(_)
                    )
                }),
            "producer" => cfg.steps[0]
                .result_manifests
                .as_mut()
                .unwrap()
                .retain(|item| matches!(item, dagrun::model::ResultManifest::ManifestCell(_))),
            "compatibility" => plan.compatibility_selected = true,
            "expected-duplicate" => {
                plan.expected_e2e_plan_json =
                    serde_json::json!({"schema":1,"cells":[identity(),identity()]}).to_string()
            }
            _ => unreachable!(),
        }
        plan.dag_json = dag_to_json(&cfg);
        let bytes = serde_json::to_vec(&plan).unwrap();
        let mut row = row.clone();
        row.extra.insert(
            "constructed_plan".into(),
            serde_json::to_value(ConstructedPlanArtifact {
                path: format!(
                    "ignored/validate/artifacts/{}/constructed-plan.json",
                    plan.run_id
                ),
                sha256: hex_digest(&bytes),
                bytes: bytes.len() as u64,
            })
            .unwrap(),
        );
        assert!(
            row.verify_schema10_artifact_bytes(&bytes, &cells, &tests)
                .is_err(),
            "{mutation} shrank the independently selected population"
        );
    }
}

fn exact_rng_population(cells: &[CellIdentity], count: usize) -> bool {
    let expected = [
        ("verify", "ptrace"),
        ("verify", "kvm"),
        ("replay", "ptrace"),
    ]
    .map(|(mode, backend)| CellIdentity {
        lane: "portable".into(),
        category: "c-programs".into(),
        test: "c-programs/random-readv-stream".into(),
        mode: mode.into(),
        backend: backend.into(),
    })
    .into_iter()
    .collect::<BTreeSet<_>>();
    let unique = cells.iter().cloned().collect::<BTreeSet<_>>();
    cells.len() == count
        && unique.len() == count
        && unique
            .into_iter()
            .filter(|cell| cell.test == "c-programs/random-readv-stream")
            .collect::<BTreeSet<_>>()
            == expected
}

/// The `e2e.manifest_backend_parity_c` command exactly as ci/dag/validate.json
/// emitted it at 422f3f3a4, before the Buck release-build names were forwarded.
/// These are retained bytes rather than a derivation from the reader's constants.
const PRE_RELEASE_ENV_PARITY_COMMAND: &str = r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY --env CI --env HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER --env HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER --env NEXTEST_TEST_THREADS --env VALIDATE_RUN_STATE -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && hermit_payload=$1 && shift && if [ "$#" -gt 0 ]; then printf -v hermit_extra '\'' %q'\'' "$@"; hermit_payload+=$hermit_extra; fi && exec bash -c "$hermit_payload"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --parity-reference ptrace --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml"'"########;

/// The two backend-parity-c selector commands exactly as ci/dag/validate.json
/// emitted them from e8007f971a72c5fd92fcf3e17e41f38811c3cac3 through
/// 3f66a249b30fada86b81e722b8e5439ac0789f8e, the last Hermit commit whose
/// manifests declared that bucket; the commands are byte-identical at both.
/// Slice S6 of <https://github.com/rrnewton/hermit/issues/3301> folded the
/// bucket into c-programs, so no generator emits these any more. Plans
/// published with them must keep reading, and these retained bytes, rather
/// than a derivation from the reader's constants, are what prove it.
const LAST_LIVE_PORTABLE_PARITY_SELECTOR: &str = r########"./ci/hermetic/run-in-pinned-root.sh --src . --out ignored/hermetic/split --src-rw --cargo-home ignored/hermetic/split/cargo --env CARGO_BUILD_JOBS --env DAGRUN_STEP_STARTED_MONOTONIC_NS --env DAGRUN_TEST_COUNTS_PATH --env E2E_BUILD_ROOT --env E2E_KERNEL_VERSION --env E2E_MACHINE_SHORTNAME --env E2E_RESULT_ROOT --env E2E_RUN_ID --env HERMIT_E2E_EMPTY_WORKDIR --env HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT --env L4_REPS --env PR_NUMBER --env SUPER_REPETITIONS --env THIRD_PARTY_BUILD_JOBS --env VALIDATE_VERBOSITY --env CI --env HERMIT_EPOCH --env E2E_PARITY_POST_PASS --env E2E_PARITY_SELECT --env HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER --env HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER --env HERMIT_VALIDATE_RELEASE_BUILD_MODE --env HERMIT_VALIDATE_BUCK_DOTSLASH --env NEXTEST_TEST_THREADS --env VALIDATE_RUN_STATE -- bash -c '/src/ci/hermetic/assert-no-network.sh && /src/ci/hermetic/assert-build-dependencies.sh && hermit_payload=$1 && shift && if [ "$#" -gt 0 ]; then printf -v hermit_extra '\'' %q'\'' "$@"; hermit_payload+=$hermit_extra; fi && exec bash -c "$hermit_payload"' bash 'export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml"'"########;
const LAST_LIVE_HOSTED_PARITY_SELECTOR: &str = r########"export PATH="$PWD/ci/rust-script-bin:$PATH"; export HERMIT_RUST_SCRIPT_ARTIFACT_ROOT="$PWD/target/ci/rust-scripts"; export HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED=1; ./ci/run-with-hermit-e2e-artifact.sh --require-install target/debug/test-harness run --lane portable --category backend-parity-c --ci-only --allow-empty --prebuilt --results "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/results.jsonl" --junit "$E2E_RESULT_ROOT/portable/manifest_backend_parity_c/junit.xml""########;

/// The retired backend-parity-c id of every live id the fold renamed.
fn folded_ids() -> BTreeMap<String, String> {
    let root = crate::git_environment::checkout_root();
    let retired = crate::retired_ids::RetiredIds::load(&root).unwrap();
    let retirement = retired.retirement("backend-parity-c").unwrap();
    assert_eq!(
        retirement.last_live_commit,
        "3f66a249b30fada86b81e722b8e5439ac0789f8e"
    );
    retirement
        .ids
        .iter()
        .map(|(old, new)| (new.clone(), old.clone()))
        .collect()
}

/// The expected plan as a plan published before the fold carried it: every
/// folded cell restored to its retired backend-parity-c identity.
fn pre_fold_expected_json(expected_json: &str) -> String {
    let folded = folded_ids();
    let mut expected: Value = serde_json::from_str(expected_json).unwrap();
    let mut restored = 0;
    for cell in expected["cells"].as_array_mut().unwrap() {
        if let Some(old) = folded.get(cell["test"].as_str().unwrap()) {
            assert_eq!(cell["category"], "c-programs");
            cell["category"] = "backend-parity-c".into();
            cell["test"] = old.as_str().into();
            restored += 1;
        }
    }
    // 276 portable and 3 privileged folded cells, plus the DBT verify cells slice
    // S13 (https://github.com/rrnewton/hermit/issues/3301) selected for the folded
    // tests c-programs/pid-probe (portable) and c-programs/cpuid-probe (privileged).
    assert_eq!(restored, 281, "277 portable and 4 privileged folded cells");
    serde_json::to_string(&expected).unwrap()
}

/// `live` as a plan published before the fold selected it: every folded cell
/// restored to its retired identity, the portable folded cells owned again by
/// their own selector step `tag` running the retained `command`, and each
/// privileged c-programs step, which owned only folded cells, renamed back in
/// place.
fn pre_fold_dag(live: &DagConfig, tag: &str, command: &str) -> DagConfig {
    use dagrun::model::ResultManifest;
    let folded = folded_ids();
    let mut cfg = live.clone();
    let mut restored_steps = Vec::new();
    let mut renamed_tags = BTreeMap::new();
    for step in &mut cfg.steps {
        let live_tag = step.tag();
        let selects_c_programs = step
            .manifest
            .as_ref()
            .is_some_and(|manifest| manifest.category == "c-programs");
        let Some(manifests) = step.result_manifests.as_mut() else {
            continue;
        };
        let mut restored = Vec::new();
        for item in manifests.iter_mut() {
            if let ResultManifest::ManifestCell(cell) = item {
                if let Some(old) = cell.test.as_ref().and_then(|test| folded.get(test)) {
                    cell.category = "backend-parity-c".into();
                    cell.test = Some(old.clone());
                    restored.push(cell.clone());
                }
            }
        }
        if restored.is_empty() || !selects_c_programs {
            continue;
        }
        let retired_tag = live_tag.replace("c_programs", "backend_parity_c");
        let owns_only_folded = manifests.iter().all(|item| match item {
            ResultManifest::ManifestCell(cell) => cell.category == "backend-parity-c",
            ResultManifest::StructuredTestResults(_) => true,
        });
        if owns_only_folded {
            assert!(live_tag.starts_with("privileged"), "{live_tag}");
            for item in manifests.iter_mut() {
                if let ResultManifest::StructuredTestResults(producer) = item {
                    producer.owner = retired_tag.clone();
                }
            }
            step.job = step.job.replace("c_programs", "backend_parity_c");
            renamed_tags.insert(live_tag.clone(), retired_tag.clone());
            step.manifest.as_mut().unwrap().category = "backend-parity-c".into();
            if step.fail_fast_family.is_some() {
                step.fail_fast_family = Some(retired_tag);
            }
            continue;
        }
        assert_eq!(retired_tag, tag);
        let mut producer = manifests
            .iter()
            .find_map(|item| match item {
                ResultManifest::StructuredTestResults(producer) => Some(producer.clone()),
                ResultManifest::ManifestCell(_) => None,
            })
            .unwrap();
        producer.owner = tag.to_owned();
        manifests.retain(|item| {
            !matches!(item, ResultManifest::ManifestCell(cell) if cell.category == "backend-parity-c")
        });
        let mut retired = step.clone();
        retired.job = tag.split_once('.').unwrap().1.to_owned();
        retired.cmd = command.to_owned();
        retired.manifest.as_mut().unwrap().category = "backend-parity-c".into();
        retired.result_manifests = Some(
            restored
                .into_iter()
                .map(ResultManifest::ManifestCell)
                .chain([ResultManifest::StructuredTestResults(producer)])
                .collect(),
        );
        if retired.fail_fast_family.is_some() {
            retired.fail_fast_family = Some(tag.to_owned());
        }
        restored_steps.push(retired);
    }
    assert_eq!(restored_steps.len(), 1, "{tag}");
    // A renamed privileged step keeps its dependents: they named the retired
    // tag before the fold.
    for step in &mut cfg.steps {
        for dep in &mut step.deps {
            if let Some(retired) = renamed_tags.get(dep.as_str()) {
                *dep = retired.clone();
            }
        }
    }
    cfg.steps.extend(restored_steps);
    cfg
}

/// Give each hosted-portable manifest step its local twin's manifest cells,
/// as every hosted plan constructed before the hosted KVM exclusion did.
fn restore_pre_exclusion_ownership(cfg: &mut DagConfig, generated: &DagConfig) {
    for step in &mut cfg.steps {
        if step.manifest.is_none() || step.labels != ["hosted-portable"] {
            continue;
        }
        let local_tag = step.tag().strip_suffix("_on_host").unwrap().to_owned();
        let local = generated
            .steps
            .iter()
            .find(|candidate| candidate.tag() == local_tag)
            .unwrap();
        let mut manifests = local
            .result_manifests
            .iter()
            .flatten()
            .filter(|manifest| matches!(manifest, dagrun::model::ResultManifest::ManifestCell(_)))
            .cloned()
            .collect::<Vec<_>>();
        manifests.extend(
            step.result_manifests
                .iter()
                .flatten()
                .filter(|manifest| {
                    !matches!(manifest, dagrun::model::ResultManifest::ManifestCell(_))
                })
                .cloned(),
        );
        step.result_manifests = Some(manifests);
    }
}

// Use the real generated graph and label selection, including the pinned-root
// wrapper additions. The small report fixture above intentionally remains a
// synthetic single-cell input; it does not cover generated command bytes.
//
// Since the backend-parity-c bucket was folded into c-programs (slice S6 of
// https://github.com/rrnewton/hermit/issues/3301), the live graph selects no
// parity relation, and a published plan from before the fold is rebuilt from
// the live graph through retired-ids.json with the selector bytes that the last
// pre-fold generator emitted.
fn generated_plan_populations_preserve_command_policy() {
    let root = crate::git_environment::checkout_root();
    let generated = crate::validation_dag::generate(&root).unwrap();
    assert_eq!(
        crate::validation_dag::canonical_text(&generated),
        std::fs::read_to_string(root.join("ci/dag/validate.json")).unwrap()
    );
    let expected_json = std::fs::read_to_string(root.join("ci/expected-e2e-plan.json")).unwrap();
    let raw_expected = crate::validation_dag::expected_cells_from_json(&expected_json)
        .unwrap()
        .iter()
        .map(exact_identity)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    // The frozen 856-cell population gains precisely these three portable RNG
    // cells, and slice S13 of https://github.com/rrnewton/hermit/issues/3301 adds
    // 41 more (26 DBT and 13 ptrace verify cells, and one DBT and one ptrace
    // custom cell for c-programs/io-uring-fallback) when it moves the retired DBT
    // backend-parity matrix onto manifest cells. Preserve raw cardinality as well
    // as the set: duplicates are not cells. Fold 1 of
    // https://github.com/rrnewton/hermit/issues/3448 adds the 189 portable
    // ptrace verify cells of the strict compatibility corpus, which ran as
    // generated compat.<label> nodes before; every plan below carries them on
    // both sides, so only its count moves.
    let compat = crate::timeouts::STRICT_COMPAT_FOLD_2026_10_01_SELECTED_CI_CELL_COUNT;
    let total = 900 + compat;
    assert!(exact_rng_population(&raw_expected, total));
    let expected_cells = raw_expected.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(expected_cells.len(), total);
    let rng = raw_expected
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.test == "c-programs/random-readv-stream")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(rng.len(), 3);
    let mut renamed = raw_expected.clone();
    renamed[rng[0]].test = "c-programs/wrong-rng-identity".into();
    assert!(!exact_rng_population(&renamed, total));
    let mut duplicate = raw_expected.clone();
    duplicate[rng[0]] = duplicate[rng[1]].clone();
    assert!(!exact_rng_population(&duplicate, total));
    let mut missing = raw_expected.clone();
    missing.remove(rng[0]);
    assert!(!exact_rng_population(&missing, total));
    // Today's hosted-portable plan omits the KVM cells, because GitHub-hosted
    // runners have no PMU; its commands carry the exclusion and its steps own
    // exactly the remaining portable cells.
    let hosted_now =
        dagrun::select_steps_by_labels(&generated, &["hosted-portable".to_owned()]).unwrap();
    let current_hosted = ConstructedValidationPlanV10 {
        schema: 1,
        run_id: "generated-hosted-portable-current".into(),
        hermit_sha: "a".repeat(40),
        path: ValidatePath::Full,
        compatibility_selected: true,
        dag_json: dag_to_json(&hosted_now),
        expected_e2e_plan_json: expected_json.clone(),
    };
    let hosted_cells = expected_cells
        .iter()
        .filter(|cell| cell.lane == "portable" && cell.backend != "kvm")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(hosted_cells.len(), 893 + compat - 241);
    assert_eq!(current_hosted.planned_cells().unwrap(), hosted_cells);
    assert_eq!(
        current_hosted.planned_backend_parity_relations().unwrap(),
        Vec::new()
    );
    // A hosted step that keeps the exclusion flag but owns the KVM cells, or
    // drops the flag but still omits them, no longer matches its selector.
    for strip_flag in [false, true] {
        let mut changed = hosted_now.clone();
        if strip_flag {
            for step in &mut changed.steps {
                step.cmd = step.cmd.replace(" --exclude-backend kvm", "");
            }
        } else {
            restore_pre_exclusion_ownership(&mut changed, &generated);
        }
        let plan = ConstructedValidationPlanV10 {
            dag_json: dag_to_json(&changed),
            ..current_hosted.clone()
        };
        assert!(
            plan.planned_cells()
                .unwrap_err()
                .ends_with("result ownership differs from its expected manifest selection"),
            "strip_flag={strip_flag}"
        );
    }
    let pre_fold_json = pre_fold_expected_json(&expected_json);
    let pre_fold_cells = crate::validation_dag::expected_cells_from_json(&pre_fold_json)
        .unwrap()
        .iter()
        .map(exact_identity)
        .collect::<Result<BTreeSet<_>, _>>()
        .unwrap();
    assert_eq!(pre_fold_cells.len(), total);
    for (label, tag, retained_command, cell_count) in [
        (
            "full",
            "e2e.manifest_backend_parity_c",
            LAST_LIVE_PORTABLE_PARITY_SELECTOR,
            total,
        ),
        (
            "hosted-portable",
            "e2e.manifest_backend_parity_c_on_host",
            LAST_LIVE_HOSTED_PARITY_SELECTOR,
            893 + compat,
        ),
    ] {
        let mut live = dagrun::select_steps_by_labels(&generated, &[label.to_owned()]).unwrap();
        if label == "hosted-portable" {
            // The current hosted plan, which omits KVM cells, is read above.
            // Every hosted plan published before that exclusion had each
            // hosted manifest step own its local twin's cells, KVM included,
            // and carried no exclusion flag; the fold-era plans below are
            // rebuilt from that shape.
            restore_pre_exclusion_ownership(&mut live, &generated);
            for step in &mut live.steps {
                step.cmd = step.cmd.replace(" --exclude-backend kvm", "");
            }
        }
        let live_selected = expected_cells
            .iter()
            .filter(|cell| label == "full" || cell.lane == "portable")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(live_selected.len(), cell_count);
        assert!(exact_rng_population(&live_selected, cell_count));
        // The live plan reads with every cell and no parity relation, and a
        // reference flag planted on its c-programs selector is refused.
        let live_plan = ConstructedValidationPlanV10 {
            schema: 1,
            run_id: format!("generated-{label}-live"),
            hermit_sha: "a".repeat(40),
            path: ValidatePath::Full,
            compatibility_selected: true,
            dag_json: dag_to_json(&live),
            expected_e2e_plan_json: expected_json.clone(),
        };
        assert_eq!(live_plan.planned_cells().unwrap(), live_selected);
        assert_eq!(
            live_plan.planned_backend_parity_relations().unwrap(),
            Vec::new()
        );
        let live_tag = tag.replace("backend_parity_c", "c_programs");
        let mut planted = live.clone();
        let step = planted
            .steps
            .iter_mut()
            .find(|step| step.tag() == live_tag)
            .unwrap();
        assert_eq!(step.cmd.matches("--prebuilt --results").count(), 1);
        step.cmd = step.cmd.replace(
            "--prebuilt --results",
            "--prebuilt --parity-reference ptrace --results",
        );
        let mut planted_plan = live_plan.clone();
        planted_plan.dag_json = dag_to_json(&planted);
        assert_eq!(
            planted_plan.planned_cells().unwrap_err(),
            format!("{live_tag} has unrecognized backend parity policy")
        );

        let selected = pre_fold_dag(&live, tag, retained_command);
        let expected_selected = pre_fold_cells
            .iter()
            .filter(|cell| label == "full" || cell.lane == "portable")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(expected_selected.len(), cell_count);
        assert!(exact_rng_population(&expected_selected, cell_count));
        for active in [false, true] {
            let mut cfg = selected.clone();
            let index = cfg.steps.iter().position(|step| step.tag() == tag).unwrap();
            assert_eq!(cfg.steps[index].jobs_flag.as_deref(), Some("--jobs"));
            let command = &mut cfg.steps[index].cmd;
            assert!(command.matches("--parity-reference ptrace ").count() <= 1);
            *command = command.replace("--parity-reference ptrace ", "");
            assert_eq!(command.matches("--prebuilt --results").count(), 1);
            if active {
                *command = command.replace(
                    "--prebuilt --results",
                    "--prebuilt --parity-reference ptrace --results",
                );
            }
            let plan = ConstructedValidationPlanV10 {
                schema: 1,
                run_id: format!(
                    "generated-{label}-{}",
                    if active { "parity" } else { "ordinary" }
                ),
                hermit_sha: "a".repeat(40),
                path: ValidatePath::Full,
                compatibility_selected: true,
                dag_json: dag_to_json(&cfg),
                expected_e2e_plan_json: pre_fold_json.clone(),
            };
            assert_eq!(plan.planned_cells().unwrap(), expected_selected);
            if !active {
                // The retained last-live bytes are the ordinary spelling.
                assert_eq!(
                    cfg.steps
                        .iter()
                        .filter(|step| step.cmd == retained_command)
                        .count(),
                    1
                );
            }
            let expected_relations = expected_selected
                .iter()
                .filter(|cell| {
                    active
                        && cell.lane == "portable"
                        && cell.category == "backend-parity-c"
                        && cell.mode == "verify"
                        && cell.backend != "ptrace"
                })
                .cloned()
                .map(BackendParityRelation::ptrace)
                .collect::<Vec<_>>();
            assert_eq!(expected_relations.len(), if active { 174 } else { 0 });
            assert_eq!(
                plan.planned_backend_parity_relations().unwrap(),
                expected_relations
            );
            if active {
                for (backend, count) in [("kvm", 75), ("liteinst", 97), ("sabre", 1), ("dbt", 1)] {
                    assert_eq!(
                        expected_relations
                            .iter()
                            .filter(|r| r.candidate.backend == backend)
                            .count(),
                        count
                    );
                }
            }
            if let Some(destination) = std::env::var_os("HERMIT_SCHEMA10_PLAN_FIXTURE_OUTPUT") {
                use std::io::Write;
                let destination = std::path::PathBuf::from(destination);
                assert!(destination.is_absolute());
                std::fs::create_dir_all(&destination).unwrap();
                let state = if active { "parity" } else { "ordinary" };
                for (suffix, bytes) in [
                    ("plan.json", serde_json::to_vec(&plan).unwrap()),
                    (
                        "populations.json",
                        serde_json::to_vec(&serde_json::json!({
                            "cells":expected_selected, "backend_parity":expected_relations
                        }))
                        .unwrap(),
                    ),
                ] {
                    let mut output = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(destination.join(format!("{label}-{state}-{suffix}")))
                        .unwrap();
                    output.write_all(&bytes).unwrap();
                }
            }
            for mutation in [
                "prefix",
                "suffix",
                "selector",
                "unknown-node",
                "raw-command",
                "legacy-guard",
                "missing-env",
                "duplicate-env",
                "half-release-env",
                "current-literal-jobs",
            ] {
                if (mutation == "unknown-node" && !active)
                    || (label != "full"
                        && matches!(
                            mutation,
                            "raw-command"
                                | "legacy-guard"
                                | "missing-env"
                                | "duplicate-env"
                                | "half-release-env"
                                | "current-literal-jobs"
                        ))
                {
                    continue;
                }
                let mut changed = cfg.clone();
                let step = &mut changed.steps[index];
                match mutation {
                    "prefix" => step.cmd.insert_str(0, "true; "),
                    "suffix" => step.cmd.push_str(" --planted"),
                    "selector" => step.manifest.as_mut().unwrap().backend = Some("kvm".into()),
                    "unknown-node" => step.job.push_str("_unknown"),
                    "raw-command" => {
                        step.cmd = if active {
                            crate::backend_parity_policy::PORTABLE_PARITY_COMMAND
                        } else {
                            crate::backend_parity_policy::PORTABLE_ORDINARY_COMMAND
                        }
                        .to_owned()
                    }
                    "legacy-guard" => {
                        let quote = |value: &str| format!("'{}'", value.replace('\'', r"'\''"));
                        let current = quote(crate::validation_dag::PINNED_ROOT_COMMAND_GUARD);
                        let legacy = quote(crate::validation_dag::LEGACY_PINNED_ROOT_COMMAND_GUARD);
                        assert_eq!(step.cmd.matches(&current).count(), 1);
                        step.cmd = step.cmd.replace(&current, &legacy);
                    }
                    "missing-env" => {
                        assert_eq!(step.cmd.matches(" --env CI ").count(), 1);
                        step.cmd = step.cmd.replace(" --env CI ", " ");
                    }
                    "duplicate-env" => {
                        assert_eq!(step.cmd.matches(" --env CI ").count(), 1);
                        step.cmd = step.cmd.replace(" --env CI ", " --env CI --env CI ");
                    }
                    // Only the complete older run is a historical spelling.
                    "half-release-env" => {
                        let dotslash = " --env HERMIT_VALIDATE_BUCK_DOTSLASH ";
                        assert_eq!(step.cmd.matches(dotslash).count(), 1);
                        step.cmd = step.cmd.replace(dotslash, " ");
                    }
                    // The literal worker count predates the release-build names,
                    // so no generator ever wrote both.
                    "current-literal-jobs" => {
                        assert_eq!(step.cmd.matches(" --results ").count(), 1);
                        step.cmd = step.cmd.replace(" --results ", " --jobs 8 --results ");
                    }
                    _ => unreachable!(),
                }
                let mut changed_plan = plan.clone();
                changed_plan.dag_json = dag_to_json(&changed);
                assert!(
                    changed_plan.planned_cells().is_err(),
                    "{label}/{active}/{mutation}"
                );
                assert!(
                    changed_plan.planned_backend_parity_relations().is_err(),
                    "{label}/{active}/{mutation}"
                );
            }
            // Plans retained before the parity post-pass names or the
            // release-build names were forwarded must keep reading with the
            // same cells and parity relations.
            for (spelling, literal_jobs) in [
                ("pre-epoch-env", false),
                ("pre-release-env", false),
                ("pre-release-env-jobs", true),
            ] {
                if label != "full" {
                    continue;
                }
                let mut previous = cfg.clone();
                let step = &mut previous.steps[index];
                if spelling == "pre-epoch-env" {
                    for name in crate::backend_parity_policy::PARITY_POST_PASS_ENV {
                        let word = format!(" --env {name} ");
                        assert_eq!(step.cmd.matches(&word).count(), 1, "{name}");
                        step.cmd = step.cmd.replacen(&word, " ", 1);
                    }
                } else {
                    assert_ne!(step.cmd, PRE_RELEASE_ENV_PARITY_COMMAND);
                    step.cmd = if active {
                        PRE_RELEASE_ENV_PARITY_COMMAND.to_owned()
                    } else {
                        PRE_RELEASE_ENV_PARITY_COMMAND.replacen("--parity-reference ptrace ", "", 1)
                    };
                }
                if literal_jobs {
                    assert_eq!(step.cmd.matches(" --results ").count(), 1);
                    step.cmd = step.cmd.replace(" --results ", " --jobs 8 --results ");
                }
                let mut previous_plan = plan.clone();
                previous_plan.dag_json = dag_to_json(&previous);
                assert_eq!(
                    previous_plan.planned_cells().unwrap(),
                    expected_selected,
                    "{label}/{active}/{spelling}"
                );
                assert_eq!(
                    previous_plan.planned_backend_parity_relations().unwrap(),
                    expected_relations,
                    "{label}/{active}/{spelling}"
                );
            }
        }
    }
}

#[test]
fn focused_profile_round_trips_without_accepting_unknown_spellings() {
    for name in [
        "quick",
        "full",
        "super",
        "cell-requalification",
        "only-quick",
        "only-full",
        "only-portable",
        "only-hosted-portable",
    ] {
        let encoded = serde_json::to_string(name).unwrap();
        let path: ValidatePath = serde_json::from_str(&encoded).unwrap();
        assert_eq!(path.as_str(), name);
        assert_eq!(serde_json::to_string(&path).unwrap(), encoded);
    }
    for unknown in [
        "cellrequalification",
        "targeted",
        "unknown-profile",
        "only-custom",
        "only-hosted",
        "only-",
    ] {
        assert!(serde_json::from_value::<ValidatePath>(unknown.into()).is_err());
    }
}

#[test]
fn selected_only_profiles_keep_schema_and_scope_at_the_verifier_boundary() {
    for path in [
        ValidatePath::OnlyQuick,
        ValidatePath::OnlyFull,
        ValidatePath::OnlyPortable,
        ValidatePath::OnlyHostedPortable,
    ] {
        let (mut row, plan, cells, tests) = fixture_with_path(
            parity(vec![completed(1, BackendParityVerdict::Matched)]),
            path,
        );
        row.selection_mode = Some("only".into());
        row.full_coverage = Some(false);
        row.verify_schema10_artifact_bytes(&plan, &cells, &tests)
            .unwrap()
            .unwrap();
        assert_eq!(row.profile.as_deref(), Some(path.as_str()));
        assert_eq!(row.full_coverage, Some(false));
        for mode in [None, Some("label"), Some("selected"), Some("full")] {
            let mut changed = row.clone();
            changed.selection_mode = mode.map(str::to_owned);
            assert!(
                changed
                    .verify_schema10_artifact_bytes(&plan, &cells, &tests)
                    .is_err()
            );
        }
        for claimed_profile in ["full", "quick"] {
            let mut changed = row.clone();
            changed.profile = Some(claimed_profile.into());
            assert!(
                changed
                    .verify_schema10_artifact_bytes(&plan, &cells, &tests)
                    .is_err()
            );
        }
        for schema in [8, 9] {
            let mut changed = row.clone();
            changed.schema_version = Some(schema);
            assert!(changed.cell_results_evidence().is_none());
            assert!(changed.cell_results_validate_path().is_none());
            if schema == 9 {
                assert!(changed.test_results_evidence().is_err());
            }
        }
    }
}

#[test]
fn focused_artifacts_require_the_same_profile_at_every_identity_boundary() {
    let path = serde_json::from_value::<ValidatePath>("cell-requalification".into()).unwrap();
    let (mut row, plan, cells, tests) = fixture_with_path(
        parity(vec![completed(1, BackendParityVerdict::Matched)]),
        path,
    );
    row.selection_mode = Some("targeted".into());
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert!(verified.full_backend_parity && verified.full_test_results);
    assert_eq!(verified.observations.len(), 3);
    assert!(verified.missing_cells.is_empty() && verified.missing_backend_parity.is_empty());
    assert_eq!(row.profile.as_deref(), Some("cell-requalification"));
    retain_fixture("matched-requalification", &row, &plan, &cells, &tests);

    for boundary in [
        "row",
        "plan",
        "cell-summary",
        "test-summary",
        "test-artifact",
    ] {
        let mut changed_row = serde_json::to_value(&row).unwrap();
        let mut changed_plan = plan.clone();
        let mut changed_tests = tests.clone();
        match boundary {
            "row" => changed_row["profile"] = "full".into(),
            "plan" => {
                let mut value: Value = serde_json::from_slice(&plan).unwrap();
                value["path"] = "full".into();
                changed_plan = serde_json::to_vec(&value).unwrap();
                changed_row["constructed_plan"]["sha256"] = hex_digest(&changed_plan).into();
                changed_row["constructed_plan"]["bytes"] = (changed_plan.len() as u64).into();
            }
            "cell-summary" => changed_row["cell_results"]["path"] = "full".into(),
            "test-summary" => changed_row["test_results"]["path"] = "full".into(),
            "test-artifact" => {
                let mut value: TestResultArtifactRow = serde_json::from_slice(&tests).unwrap();
                value.path = ValidatePath::Full;
                changed_tests = serde_json::to_vec(&value).unwrap();
                changed_tests.push(b'\n');
                changed_row["test_results"]["artifact"]["sha256"] =
                    hex_digest(&changed_tests).into();
            }
            _ => unreachable!(),
        }
        let changed_row: HistoryRow = serde_json::from_value(changed_row).unwrap();
        let expected = match boundary {
            "row" | "plan" => "schema 10 constructed plan differs from its row identity",
            "cell-summary" => "schema 10 cell evidence differs from the exact clean row identity",
            "test-summary" => "schema 9 test_results path differs from row profile",
            "test-artifact" => "schema 9 test-results artifact row 1 has wrong validation path",
            _ => unreachable!(),
        };
        assert_eq!(
            changed_row
                .verify_schema10_artifact_bytes(&changed_plan, &cells, &changed_tests)
                .expect_err("mismatched profile must be refused"),
            expected,
            "wrong refusal for {boundary} profile"
        );
    }
}

/// The binding guard must actually RUN on a real row, and its attempt arm must
/// actually be able to fire.
///
/// ⚠️ THE FIVE-ORDINAL PROBE BELOW IS THE REVIEWER'S, REPRODUCED. At
/// `c4b692ada` the guard passed the binding's own ordinal in as the value it
/// checked against, so attempts 1, 2, 7, 999 and `u64::MAX` all returned `Ok`
/// while producing five distinct bindings. Every one of them must now be
/// REFUSED, because the ledger row records the ordinal independently and the
/// guard compares against that instead of against the binding itself.
#[test]
fn the_live_row_decode_refuses_a_compared_verdict_whose_binding_is_inconsistent() {
    let (row, _plan, _cells, _tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));

    // Control: the untouched fixture decodes and really does carry a compared
    // verdict with a binding and a recorded ordinal.
    let evidence = row
        .schema10_cell_results()
        .expect("the untouched fixture must decode")
        .expect("schema 10 row carries cell results");
    assert!(matches!(
        evidence.cells[0].cell_verdict,
        CellVerdict::ComparedAndMatched { .. }
    ));
    assert_eq!(evidence.cells[0].selected_attempt, Some(1));
    assert_eq!(
        evidence.cells[0]
            .evidence_binding
            .as_ref()
            .expect("the producer bound the compared verdict")
            .selected_attempt,
        1
    );
    assert_eq!(evidence.bound_attempts().unwrap().len(), 1);

    let decode = |mutate: &dyn Fn(&mut Value)| -> String {
        let mut row = row.clone();
        let mut cells = serde_json::to_value(row.cell_results.as_ref().unwrap()).unwrap();
        mutate(&mut cells["cells"][0]);
        row.cell_results = Some(serde_json::from_value(cells).unwrap());
        row.schema10_cell_results()
            .expect_err("the live decode must refuse this row")
    };

    // THE REVIEWER'S PROBE. Each of these was accepted at c4b692ada.
    for ordinal in [2_u64, 7, 999, u64::MAX] {
        let error = decode(&|cell| {
            cell["evidence_binding"]["selected_attempt"] = Value::from(ordinal);
        });
        assert!(
            error.contains(&format!("binds attempt {ordinal} while the row records 1")),
            "attempt {ordinal} was not refused: {error}"
        );
    }
    // And the same ordinal on the ROW rather than the binding is refused too,
    // so the check cannot be satisfied by moving the lie to the other operand.
    for ordinal in [2_u64, 7, 999, u64::MAX] {
        let error = decode(&|cell| {
            cell["selected_attempt"] = Value::from(ordinal);
        });
        assert!(
            error.contains(&format!("binds attempt 1 while the row records {ordinal}")),
            "row ordinal {ordinal} was not refused: {error}"
        );
    }

    // MISSING: the producer stopped writing the binding.
    let error = decode(&|cell| {
        cell.as_object_mut().unwrap().remove("evidence_binding");
    });
    assert!(error.contains("carries no evidence binding"), "{error}");

    // MISSING the row's own operand, which would otherwise let the attempt arm
    // quietly stop checking anything again.
    let error = decode(&|cell| {
        cell.as_object_mut().unwrap().remove("selected_attempt");
    });
    assert!(error.contains("no selected_attempt"), "{error}");

    // WRONG CELL: internally well-formed, evidence for another cell.
    let foreign = CellEvidenceBinding::for_validate_compared(
        &evidence.run_id,
        &CellIdentity {
            test: "somewhere/else".into(),
            ..identity()
        },
        &evidence.hermit_sha,
        1,
    );
    let error = decode(&|cell| {
        cell["evidence_binding"] = serde_json::to_value(&foreign).unwrap();
    });
    assert!(error.contains("is for cell"), "{error}");

    // FOREIGN RUN.
    let other_run = CellEvidenceBinding::for_validate_compared(
        "some-other-run",
        &identity(),
        &evidence.hermit_sha,
        1,
    );
    let error = decode(&|cell| {
        cell["evidence_binding"] = serde_json::to_value(&other_run).unwrap();
    });
    assert!(error.contains("is for run"), "{error}");

    // A VERDICT THAT COMPARED NOTHING MUST NOT CARRY ONE.
    let error = decode(&|cell| {
        cell["cell_verdict"] = serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "canonical-bitwise",
            "reason": "synthetic",
        });
    });
    assert!(
        error.contains("states no comparison yet carries an evidence binding"),
        "{error}"
    );

    // Positive control, so none of the above is passing because every mutation
    // is refused: moving BOTH operands together to the same new ordinal still
    // decodes. That is also the honest statement of the guard's limit -- it
    // establishes consistency, not that the ordinal is the right one.
    let mut good = row.clone();
    let mut cells = serde_json::to_value(good.cell_results.as_ref().unwrap()).unwrap();
    cells["cells"][0]["selected_attempt"] = Value::from(4);
    cells["cells"][0]["evidence_binding"]["selected_attempt"] = Value::from(4);
    good.cell_results = Some(serde_json::from_value(cells).unwrap());
    good.schema10_cell_results()
        .expect("a consistently rebound row must still decode");
}

fn legacy_fixture(name: &str) -> (HistoryRow, Vec<u8>, Vec<u8>, Vec<u8>) {
    let (row, plan, cells, tests): (&str, &[u8], &[u8], &[u8]) = match name {
        "ordinary-only" => (
            include_str!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/ordinary-only-row.json"
            ),
            include_bytes!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/ordinary-only-plan.json"
            ),
            include_bytes!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/ordinary-only-cells.jsonl"
            ),
            include_bytes!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/ordinary-only-tests.jsonl"
            ),
        ),
        "reference-diverged" => (
            include_str!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/reference-diverged-row.json"
            ),
            include_bytes!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/reference-diverged-plan.json"
            ),
            include_bytes!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/reference-diverged-cells.jsonl"
            ),
            include_bytes!(
                "../../../../../tests/fixtures/ledger-schema10/legacy/reference-diverged-tests.jsonl"
            ),
        ),
        _ => panic!("unknown preserved legacy fixture: {name}"),
    };
    (
        serde_json::from_str(row).unwrap(),
        plan.to_vec(),
        cells.to_vec(),
        tests.to_vec(),
    )
}

fn decode_cell_evidence(value: Value) -> Result<CellResultsEvidenceV10, String> {
    let row: HistoryRow = serde_json::from_value(value).map_err(|error| error.to_string())?;
    row.schema10_cell_results()?
        .ok_or_else(|| "fixture did not select schema 10".into())
}

#[test]
fn exact_legacy_artifacts_remain_authenticated_without_inferred_bindings() {
    for name in ["ordinary-only", "reference-diverged"] {
        let (row, plan, cells, tests) = legacy_fixture(name);
        let original = serde_json::to_value(row.cell_results.as_ref().unwrap()).unwrap();
        let verified = row
            .verify_schema10_artifact_bytes(&plan, &cells, &tests)
            .unwrap()
            .unwrap();
        let evidence = &verified.cell_results;
        assert_eq!(
            evidence.binding_contract,
            CellBindingContract::LegacyUnbound
        );
        assert_eq!(serde_json::to_value(evidence).unwrap(), original, "{name}");
        assert_eq!(evidence.run_id, "fixture-v10");
        assert_eq!(evidence.hermit_sha, "a".repeat(40));
        assert_eq!(evidence.selected, vec![identity()]);
        assert_eq!(evidence.selected_count, 1);
        assert_eq!(evidence.recorded_count, 1);
        assert_eq!(evidence.cells.len(), 1);
        assert!(matches!(
            evidence.cells[0].cell_verdict,
            CellVerdict::ComparedAndMatched { .. }
        ));
        assert_eq!(evidence.cells[0].selected_attempt, None);
        assert_eq!(evidence.cells[0].evidence_binding, None);
        let full_cells = evidence.verify_cell_artifact_bytes(&cells).unwrap();
        assert_eq!(full_cells.len(), 1);
        assert_eq!(full_cells[0].selected_attempt, None);
        assert_eq!(
            full_cells[0]
                .summary_for_contract(
                    CellBindingContract::LegacyUnbound,
                    &evidence.run_id,
                    &evidence.hermit_sha,
                )
                .unwrap(),
            evidence.cells[0]
        );
        assert!(
            evidence
                .bound_attempts()
                .unwrap_err()
                .contains("legacy-unbound")
        );
        assert!(verified.full_test_results);
        assert!(verified.missing_cells.is_empty());
        assert!(verified.missing_backend_parity.is_empty());
        assert!(verified.missing_test_producers.is_empty());
        assert!(!verified.full_backend_parity);
        assert_eq!(verified.test_results.totals.executed_tests, 1);
        assert_eq!(verified.test_results.totals.passed_tests, 1);
        assert_eq!(verified.test_results.totals.failed_tests, 0);
        assert_eq!(verified.test_results.totals.filtered_tests, 0);

        // Readability is not binding authority, even for an ordinary match.
        assert!(
            evidence
                .require_bound_compared_cells()
                .unwrap_err()
                .contains("legacy-unbound")
        );
        if name == "ordinary-only" {
            assert!(evidence.selected_backend_parity.is_empty());
            assert_eq!(verified.observations.len(), 1);
            let observation = &verified.observations[0];
            assert_eq!(observation.identity, identity());
            assert_eq!(observation.relation, ComparisonRelationV10::Ordinary);
            assert_eq!(observation.outer_attempt, None);
            assert_eq!(
                observation.verdict,
                ComparisonObservationVerdictV10::Matched
            );
        }
    }
}

/// https://github.com/rrnewton/hermit/issues/3301 removed `--parity-reference`
/// from every newly generated plan. A plan retained before that change keeps
/// verifying with the parity relations it was published with, both at the last
/// parity spelling the generator emitted and as the preserved legacy producer
/// export.
#[test]
fn plans_retained_before_issue_3301_still_verify_their_parity_relations() {
    let root = crate::git_environment::checkout_root();
    let generated = crate::validation_dag::generate(&root).unwrap();
    // Slice S6 of https://github.com/rrnewton/hermit/issues/3301 then folded
    // the two parity selectors into the c-programs pair, which selects no
    // parity relation.
    for (retired, live) in [
        ("e2e.manifest_backend_parity_c", "e2e.manifest_c_programs"),
        (
            "e2e.manifest_backend_parity_c_on_host",
            "e2e.manifest_c_programs_on_host",
        ),
    ] {
        assert!(generated.steps.iter().all(|step| step.tag() != retired));
        let step = generated
            .steps
            .iter()
            .find(|step| step.tag() == live)
            .unwrap();
        assert!(!step.cmd.contains("--parity-reference"), "{live}");
        assert!(
            !crate::backend_parity_policy::selects_ptrace_parity(step).unwrap(),
            "{live}"
        );
    }
    // The last hosted command a generator emitted is the last parity spelling
    // minus exactly the removed flag, so a plan carrying that spelling is a
    // pre-change plan. The last portable command is the reader's ordinary
    // spelling under today's pinned-root wrapper.
    assert_eq!(
        LAST_LIVE_HOSTED_PARITY_SELECTOR,
        crate::backend_parity_policy::HOSTED_ORDINARY_COMMAND
    );
    assert_eq!(
        crate::backend_parity_policy::HOSTED_PARITY_COMMAND.replacen(
            " --parity-reference ptrace",
            "",
            1
        ),
        LAST_LIVE_HOSTED_PARITY_SELECTOR
    );
    assert_eq!(
        crate::validation_dag::refresh_pinned_root_environment(
            "e2e.manifest_backend_parity_c",
            crate::backend_parity_policy::PORTABLE_ORDINARY_COMMAND
        )
        .unwrap(),
        LAST_LIVE_PORTABLE_PARITY_SELECTOR
    );
    let relation = vec![BackendParityRelation::ptrace(identity())];

    let (row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let retained: ConstructedValidationPlanV10 = serde_json::from_slice(&plan).unwrap();
    assert_eq!(
        retained.constructed_dag().unwrap().steps[0].cmd,
        crate::backend_parity_policy::HOSTED_PARITY_COMMAND
    );
    assert_eq!(
        retained.planned_backend_parity_relations().unwrap(),
        relation
    );
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert!(verified.full_backend_parity && verified.full_test_results);
    assert_eq!(verified.cell_results.selected_backend_parity, relation);
    assert!(verified.missing_backend_parity.is_empty());

    // Exact bytes a producer exported before the change, in the older
    // literal-worker-count spelling, read the same way.
    let (row, plan, cells, tests) = legacy_fixture("reference-diverged");
    let retained: ConstructedValidationPlanV10 = serde_json::from_slice(&plan).unwrap();
    assert!(
        retained.constructed_dag().unwrap().steps[0]
            .cmd
            .contains("--prebuilt --parity-reference ptrace --jobs 8 --results")
    );
    assert_eq!(
        retained.planned_backend_parity_relations().unwrap(),
        relation
    );
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert_eq!(verified.cell_results.selected_backend_parity, relation);
}

#[test]
fn legacy_reference_failure_and_unavailable_cross_comparison_remain_visible() {
    let (row, plan, cells, tests) = legacy_fixture("reference-diverged");
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert_eq!(
        verified.cell_results.binding_contract,
        CellBindingContract::LegacyUnbound
    );
    assert_eq!(verified.cell_results.selected_backend_parity.len(), 1);
    assert_eq!(verified.observations.len(), 3);
    assert!(!verified.full_backend_parity);
    let mut reference = identity();
    reference.backend = "ptrace".into();
    let expected_relations = [
        (identity(), ComparisonRelationV10::Ordinary),
        (
            reference,
            ComparisonRelationV10::Reference {
                candidate: identity(),
            },
        ),
        (
            identity(),
            ComparisonRelationV10::BackendParity {
                reference_backend: "ptrace".into(),
                record_envelope: RecordEnvelopePolicy::CrossBackendDetcoreV1,
            },
        ),
    ];
    for ((identity, relation), observation) in expected_relations.iter().zip(&verified.observations)
    {
        assert_eq!(&observation.identity, identity);
        assert_eq!(&observation.relation, relation);
        assert_eq!(observation.outer_attempt, Some(1));
    }
    assert_eq!(
        verified.observations[0].verdict,
        ComparisonObservationVerdictV10::Matched
    );
    assert_eq!(
        verified.observations[1].verdict,
        ComparisonObservationVerdictV10::Diverged
    );
    assert_eq!(
        verified.observations[2].verdict,
        ComparisonObservationVerdictV10::UnavailableWithReason {
            reason: "Cross-backend comparison unavailable; exact detail is retained in the cell artifact".into(),
        }
    );
    let full_cells = verified
        .cell_results
        .verify_cell_artifact_bytes(&cells)
        .unwrap();
    let RequiredNullable::Value(parity) = &full_cells[0].backend_parity else {
        panic!("the old artifact lost its parity evidence");
    };
    let BackendParityCellAttempt::UnavailableWithReason { reason, .. } = &parity.attempts[0] else {
        panic!("the old artifact lost its unavailable attempt");
    };
    assert_eq!(
        reason,
        "reference strict verification diverged before cross comparison"
    );
}

#[test]
fn binding_contract_presence_requires_a_known_nonnull_integer_version() {
    let (row, _plan, _cells, _tests) = legacy_fixture("ordinary-only");
    let original = serde_json::to_value(row).unwrap();
    assert_eq!(
        decode_cell_evidence(original.clone())
            .unwrap()
            .binding_contract,
        CellBindingContract::LegacyUnbound
    );
    for marker in [
        Value::Null,
        serde_json::json!(0),
        serde_json::json!(2),
        serde_json::json!(-1),
        serde_json::json!("1"),
        serde_json::json!(true),
        serde_json::json!(1.0),
        serde_json::json!([]),
        serde_json::json!({}),
    ] {
        let mut changed = original.clone();
        changed["cell_results"]["binding_contract"] = marker.clone();
        let error = decode_cell_evidence(changed)
            .expect_err("a present invalid marker is not legacy absence");
        assert!(
            error.contains("binding contract")
                || error.contains("invalid type")
                || error.contains("invalid value"),
            "marker {marker} had the wrong refusal: {error}"
        );
    }
    let mut marked = original;
    marked["cell_results"]["binding_contract"] = serde_json::json!(1);
    assert!(
        decode_cell_evidence(marked)
            .unwrap_err()
            .contains("carries no evidence binding")
    );
}

#[test]
fn legacy_compact_shape_refuses_new_keys_even_when_their_values_are_null() {
    let (row, _plan, _cells, _tests) = legacy_fixture("ordinary-only");
    let original = serde_json::to_value(row).unwrap();
    let binding = serde_json::to_value(CellEvidenceBinding::for_validate_compared(
        "fixture-v10",
        &identity(),
        &"a".repeat(40),
        1,
    ))
    .unwrap();
    for (key, value) in [
        ("selected_attempt", Value::Null),
        ("selected_attempt", serde_json::json!(1)),
        ("evidence_binding", Value::Null),
        ("evidence_binding", binding),
    ] {
        let mut changed = original.clone();
        changed["cell_results"]["cells"][0][key] = value;
        let error = decode_cell_evidence(changed).unwrap_err();
        assert!(
            error.contains("unknown field") && error.contains(key),
            "{error}"
        );
    }
}

#[test]
fn legacy_artifact_shape_refuses_new_keys_even_after_rehashing() {
    let (row, plan, cells, tests) = legacy_fixture("ordinary-only");
    let original_row = serde_json::to_value(row).unwrap();
    let original_cell: Value = serde_json::from_slice(&cells).unwrap();
    for (key, value) in [
        ("selected_attempt", Value::Null),
        ("selected_attempt", serde_json::json!(1)),
        ("evidence_binding", Value::Null),
    ] {
        let mut changed_cell = original_cell.clone();
        changed_cell[key] = value;
        let mut changed_cells = serde_json::to_vec(&changed_cell).unwrap();
        changed_cells.push(b'\n');
        let mut changed_row = original_row.clone();
        changed_row["cell_results"]["artifact"]["sha256"] =
            Value::String(hex_digest(&changed_cells));
        let changed_row: HistoryRow = serde_json::from_value(changed_row).unwrap();
        let error = changed_row
            .verify_schema10_artifact_bytes(&plan, &changed_cells, &tests)
            .unwrap_err();
        assert!(
            error.contains("unknown field") && error.contains(key),
            "{error}"
        );
    }
}

#[test]
fn removing_a_binding_contract_cannot_retain_bound_comparison_authority() {
    let (row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap()
        .unwrap();
    assert_eq!(verified.cell_results.bound_attempts().unwrap().len(), 1);
    let mut downgraded = serde_json::to_value(row).unwrap();
    downgraded["cell_results"]
        .as_object_mut()
        .unwrap()
        .remove("binding_contract");
    assert!(
        decode_cell_evidence(downgraded.clone())
            .unwrap_err()
            .contains("unknown field")
    );
    for key in ["selected_attempt", "evidence_binding"] {
        downgraded["cell_results"]["cells"][0]
            .as_object_mut()
            .unwrap()
            .remove(key);
    }
    let row_without_compact_binding: HistoryRow =
        serde_json::from_value(downgraded.clone()).unwrap();
    let error = row_without_compact_binding
        .verify_schema10_artifact_bytes(&plan, &cells, &tests)
        .unwrap_err();
    assert!(
        error.contains("unknown field") && error.contains("selected_attempt"),
        "{error}"
    );

    // Stripping every new field can yield readable old-format evidence, but
    // never silently satisfy a request for bound comparisons.
    let mut legacy_cell: Value = serde_json::from_slice(&cells).unwrap();
    legacy_cell
        .as_object_mut()
        .unwrap()
        .remove("selected_attempt");
    let mut legacy_cells = serde_json::to_vec(&legacy_cell).unwrap();
    legacy_cells.push(b'\n');
    downgraded["cell_results"]["artifact"]["sha256"] = Value::String(hex_digest(&legacy_cells));
    let downgraded: HistoryRow = serde_json::from_value(downgraded).unwrap();
    let legacy = downgraded
        .verify_schema10_artifact_bytes(&plan, &legacy_cells, &tests)
        .unwrap()
        .unwrap();
    assert_eq!(
        legacy.cell_results.binding_contract,
        CellBindingContract::LegacyUnbound
    );
    assert_eq!(legacy.observations, verified.observations);
    assert!(
        legacy
            .cell_results
            .cells
            .iter()
            .all(|cell| { cell.selected_attempt.is_none() && cell.evidence_binding.is_none() })
    );
    assert!(
        legacy
            .cell_results
            .bound_attempts()
            .unwrap_err()
            .contains("legacy-unbound")
    );
}

#[test]
fn empty_legacy_cell_evidence_cannot_satisfy_bound_attempts() {
    let (row, _plan, _cells, _tests) = legacy_fixture("ordinary-only");
    let mut raw = serde_json::to_value(row).unwrap();
    let evidence = &mut raw["cell_results"];
    evidence["selected"] = serde_json::json!([]);
    evidence["selected_backend_parity"] = serde_json::json!([]);
    evidence["cells"] = serde_json::json!([]);
    evidence["selected_count"] = serde_json::json!(0);
    evidence["recorded_count"] = serde_json::json!(0);
    evidence["population_sha256"] = Value::String(hex_digest(b"[]"));
    evidence["artifact"]["row_count"] = serde_json::json!(0);
    evidence["artifact"]["sha256"] = Value::String(hex_digest(b""));
    let evidence = decode_cell_evidence(raw).unwrap();
    assert_eq!(
        evidence.binding_contract,
        CellBindingContract::LegacyUnbound
    );
    assert!(evidence.verify_cell_artifact_bytes(b"").unwrap().is_empty());
    assert!(
        evidence
            .bound_attempts()
            .unwrap_err()
            .contains("legacy-unbound")
    );
}

#[test]
fn raw_history_rows_refuse_duplicate_binding_contract_markers() {
    // The narrow raw marker check must not discard unrelated future shapes.
    for future in [
        serde_json::json!(true),
        serde_json::json!(-1),
        serde_json::json!(u64::MAX),
        serde_json::json!(1.25),
        serde_json::json!("future"),
        serde_json::json!([1, null, {"future": true}]),
        serde_json::json!({"future": {"fields": [1, 2]}}),
    ] {
        let raw = serde_json::json!({"schema_version": 123, "cell_results": future}).to_string();
        let row: HistoryRow = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            serde_json::to_value(row.cell_results.unwrap()).unwrap(),
            future
        );
    }
    let (row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let raw = serde_json::to_string(&row).unwrap();
    let needle = "\"binding_contract\":1";
    assert_eq!(raw.matches(needle).count(), 1);
    let control: HistoryRow = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        control
            .verify_schema10_artifact_bytes(&plan, &cells, &tests)
            .unwrap()
            .unwrap()
            .cell_results
            .binding_contract,
        CellBindingContract::SelectedAttemptV1
    );
    // Every other value and retained artifact is valid. Test both orders so a
    // last-writer-wins parse cannot make the invalid first marker disappear.
    for (first, second) in [
        ("1", "1"),
        ("0", "1"),
        ("1", "0"),
        ("null", "1"),
        ("1", "null"),
    ] {
        let duplicate = format!("\"binding_contract\":{first},\"binding_contract\":{second}");
        let changed = raw.replacen(needle, &duplicate, 1);
        let error = serde_json::from_str::<HistoryRow>(&changed).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate field `binding_contract`"),
            "{first}/{second}: {error}"
        );
    }
}

#[test]
fn cpu_history_is_authenticated_without_changing_compact_verdicts() {
    let (row, plan, original, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    row.verify_schema10_artifact_bytes(&plan, &original, &tests)
        .unwrap()
        .unwrap();
    let original_value: Value = serde_json::from_slice(&original).unwrap();
    let verified = row
        .verify_schema10_artifact_bytes(&plan, &original, &tests)
        .unwrap()
        .unwrap();
    let decoded = verified
        .cell_results
        .verify_cell_artifact_bytes(&original)
        .unwrap();
    assert!(decoded[0].cpu_observation_history.is_none());
    let mut historical = serde_json::to_value(&decoded[0]).unwrap();
    for key in ["run_id", "hermit_sha", "source_tree_dirty"] {
        historical[key] = original_value[key].clone();
    }
    let mut historical_bytes = serde_json::to_vec(&historical).unwrap();
    historical_bytes.push(b'\n');
    assert_eq!(historical_bytes, original);
    let mut source = crate::cpu_evidence::tests::source_row();
    for key in [
        "run_id",
        "hermit_sha",
        "lane",
        "category",
        "test",
        "mode",
        "backend",
    ] {
        source[key] = original_value[key].clone();
    }
    let operands = &original_value["backend_parity"]["attempts"][0];
    source["attempts"] =
        serde_json::json!([operands["candidate_attempt"], operands["reference_attempt"]]);
    let observations = crate::cpu_evidence::tests::envelope(&source);
    let history = serde_json::json!({"version":1,"attempts":[{"state":"recorded","outer_attempt":1,"observations":observations}]});
    let check = |value: &Value| {
        let mut bytes = serde_json::to_vec(value).unwrap();
        bytes.push(b'\n');
        let mut parent = serde_json::to_value(&row).unwrap();
        parent["cell_results"]["artifact"]["sha256"] = hex_digest(&bytes).into();
        let parent: HistoryRow = serde_json::from_value(parent).unwrap();
        parent.verify_schema10_artifact_bytes(&plan, &bytes, &tests)
    };
    let mut supplied = original_value.clone();
    supplied["cpu_observation_history"] = history;
    let admitted = check(&supplied).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(admitted.cell_results.cells).unwrap(),
        serde_json::to_value(verified.cell_results.cells).unwrap()
    );
    for (pointer, value) in [
        ("/cpu_observation_history", Value::Null),
        ("/cpu_observation_history/version", serde_json::json!(2)),
        (
            "/cpu_observation_history/attempts/0/observations/binding/run_id",
            serde_json::json!("foreign"),
        ),
        (
            "/cpu_observation_history/attempts/0/outer_attempt",
            serde_json::json!(2),
        ),
        (
            "/cpu_observation_history/attempts/0/observations/invocations/0/command/argv",
            serde_json::json!(["foreign"]),
        ),
    ] {
        let mut bad = supplied.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(check(&bad).is_err(), "{pointer}");
    }
    let mut missing = supplied.clone();
    missing["cpu_observation_history"]["attempts"][0]["observations"]["invocations"] =
        serde_json::json!([]);
    assert!(check(&missing).is_err());
    let mut unknown = supplied.clone();
    unknown["cpu_observation_history"]["future"] = Value::Bool(true);
    assert!(check(&unknown).is_err());
    // The authenticated artifact retains typed parity attempts. Intrinsically
    // valid CPU evidence must still agree with their actual timeout flags.
    let mut contradicted = supplied.clone();
    let reference = &mut contradicted["cpu_observation_history"]["attempts"][0]["observations"]["invocations"]
        [1];
    reference["termination"] = serde_json::json!("final_wait_cpu_budget_return");
    reference["live"] = serde_json::json!({"state":"enabled","source":"agent_utils_paired_pidfd_stat_v1","registration":{"state":"unavailable","reason":"synthetic refusal"},"polls":0,"source_sample_calls":0,"valid_polls":0,"unavailable_polls":0,"first":null,"last":null,"high_water":null,"timeout_trigger":null,"last_error":null});
    let intrinsic: crate::cpu_evidence::CellCpuObservationsV1 = serde_json::from_value(
        contradicted["cpu_observation_history"]["attempts"][0]["observations"].clone(),
    )
    .unwrap();
    intrinsic.validate().unwrap();
    assert!(
        check(&contradicted)
            .unwrap_err()
            .contains("retained timed_out")
    );
    contradicted
        .as_object_mut()
        .unwrap()
        .remove("cpu_observation_history");
    assert!(check(&contradicted).is_ok());
    // Legacy absence stays exact; removing its binding contract cannot admit a new field.
    let mut legacy = original_value.clone();
    legacy.as_object_mut().unwrap().remove("selected_attempt");
    for key in ["run_id", "hermit_sha", "source_tree_dirty"] {
        legacy.as_object_mut().unwrap().remove(key);
    }
    assert!(serde_json::from_value::<LegacyCellArtifactResultV10Wire>(legacy.clone()).is_ok());
    for extension in [Value::Null, supplied["cpu_observation_history"].clone()] {
        legacy["cpu_observation_history"] = extension;
        assert!(serde_json::from_value::<LegacyCellArtifactResultV10Wire>(legacy.clone()).is_err());
    }
}

// Pure-data controls for the shared semantic verifier. These values never
// establish canonical origin or grant a durable history-proof capability.
fn finalized_raw_inputs(row: &mut HistoryRow, attempts: &[u64]) -> BTreeMap<String, Vec<u8>> {
    let id = identity();
    let mut raw = Vec::new();
    for attempt in attempts {
        raw.extend(
            serde_json::to_vec(&serde_json::json!({
                "schema":4, "run_id":row.run_id, "hermit_sha":row.commit,
                "source_tree_dirty":false, "lane":id.lane, "category":id.category,
                "test":id.test, "mode":id.mode, "backend":id.backend,
                "attempt":attempt, "outcome":"FAIL"
            }))
            .unwrap(),
        );
        raw.push(b'\n');
    }
    let inputs = BTreeMap::from([
        ("rows/results.jsonl".into(), raw),
        ("empty/results.jsonl".into(), Vec::new()),
    ]);
    let census = RawResultInputCensusV1::from_inputs(
        row.run_id.as_deref().unwrap(),
        row.commit.as_deref().unwrap(),
        &inputs,
    )
    .unwrap();
    row.extra.insert(
        "raw_result_input_census_v1".into(),
        serde_json::to_value(census).unwrap(),
    );
    row.commit_anchored = Some(true);
    row.started_at = Some("producer-start".into());
    row.finished_at = Some("producer-finish".into());
    row.result = Some("fail".into());
    inputs
}

#[test]
fn finalized_raw_verifier_preserves_partial_failure_and_checks_terminal_identity() {
    let mut row: HistoryRow = serde_json::from_value(serde_json::json!({
        "schema_version":5,"run_id":"partial-run","commit":"a".repeat(40),
        "tree_dirty":false,"executed_tests":null,"gates_expected":3,"gates_run":1,
        "unaccounted_nodes":["test.not-run"]
    }))
    .unwrap();
    let inputs = finalized_raw_inputs(&mut row, &[1, 2]);
    let encoded = serde_json::to_vec(&row).unwrap();
    assert!(
        !row.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap()
    );
    assert_eq!(serde_json::to_vec(&row).unwrap(), encoded);
    assert_eq!(row.result.as_deref(), Some("fail"));
    assert_eq!(row.executed_tests, None);
    assert_eq!(row.gates_expected, Some(3));
    assert_eq!(row.gates_run, Some(1));
    let mut no_result = row.clone();
    no_result.result = Some("no_result".into());
    assert!(
        !no_result
            .verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap()
    );
    for field in [
        "commit",
        "tree_dirty",
        "commit_anchored",
        "started_at",
        "finished_at",
        "result",
    ] {
        let mut bad = row.clone();
        match field {
            "commit" => bad.commit = Some("b".repeat(40)),
            "tree_dirty" => bad.tree_dirty = Some(true),
            "commit_anchored" => bad.commit_anchored = None,
            "started_at" => bad.started_at = Some("different-start".into()),
            "finished_at" => bad.finished_at = None,
            "result" => bad.result = Some("running".into()),
            _ => unreachable!(),
        }
        assert_eq!(
            bad.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
                .unwrap_err(),
            "finalized row does not bind a terminal clean measured run and its stamp",
            "{field}"
        );
    }
    let mut missing_run = row.clone();
    missing_run.run_id = None;
    assert_eq!(
        missing_run
            .verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap_err(),
        "finalized row omitted its run identity"
    );
    let mut wrong_run = row.clone();
    wrong_run.run_id = Some("another-run".into());
    assert_eq!(
        wrong_run
            .verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap_err(),
        "raw result census has an unsupported version or row identity"
    );
}

#[test]
fn finalized_raw_verifier_keeps_complete_census_and_error_refusals() {
    let mut row: HistoryRow = serde_json::from_value(serde_json::json!({
        "schema_version":5,"run_id":"partial-run","commit":"a".repeat(40),"tree_dirty":false
    }))
    .unwrap();
    let inputs = finalized_raw_inputs(&mut row, &[1, 2]);
    assert!(
        !row.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap()
    );
    for change in [
        "empty-file",
        "superseded-attempt",
        "renamed",
        "extra",
        "truncated",
    ] {
        let mut changed = inputs.clone();
        match change {
            "empty-file" => {
                changed.remove("empty/results.jsonl");
            }
            "superseded-attempt" => {
                let raw = changed.get_mut("rows/results.jsonl").unwrap();
                let end = raw.iter().position(|byte| *byte == b'\n').unwrap() + 1;
                raw.drain(..end);
            }
            "renamed" => {
                let bytes = changed.remove("rows/results.jsonl").unwrap();
                changed.insert("renamed/results.jsonl".into(), bytes);
            }
            "extra" => {
                changed.insert("extra/results.jsonl".into(), Vec::new());
            }
            "truncated" => {
                changed.get_mut("rows/results.jsonl").unwrap().pop();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            row.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &changed, None)
                .unwrap_err(),
            "current result population or bytes differ from the producer-finalized raw census",
            "{change}"
        );
    }
    let mut bad = row.clone();
    bad.extra.remove("raw_result_input_census_v1");
    assert!(
        bad.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap_err()
            .starts_with("finalized run has no producer-bound raw input census:")
    );
    let mut bad = row.clone();
    bad.extra.insert(
        "raw_result_input_census_error".into(),
        Value::String("publication failed".into()),
    );
    assert_eq!(
        bad.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap_err(),
        "raw result census cannot carry both proof and publication error"
    );
    let empty_inputs = finalized_raw_inputs(&mut row, &[]);
    assert_eq!(
        row.verify_finalized_raw_input_bytes(
            &"a".repeat(40),
            "producer-start",
            &empty_inputs,
            None
        )
        .unwrap_err(),
        "zero-current proof requires schema 10"
    );
}

#[test]
fn finalized_raw_verifier_checks_recorded_population_selected_attempt_and_all_artifacts() {
    let (mut row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let inputs = finalized_raw_inputs(&mut row, &[1, 2]);
    assert!(
        !row.verify_finalized_raw_input_bytes(
            &"a".repeat(40),
            "producer-start",
            &inputs,
            Some((&plan, &cells, &tests))
        )
        .unwrap()
    );
    assert_eq!(
        row.verify_finalized_raw_input_bytes(&"a".repeat(40), "producer-start", &inputs, None)
            .unwrap_err(),
        "finalized proof omitted required plan/cell/test artifact bytes"
    );
    for which in 0..3 {
        let mut artifacts = [plan.clone(), cells.clone(), tests.clone()];
        artifacts[which].push(b' ');
        assert!(
            row.verify_finalized_raw_input_bytes(
                &"a".repeat(40),
                "producer-start",
                &inputs,
                Some((&artifacts[0], &artifacts[1], &artifacts[2]))
            )
            .is_err(),
            "artifact {which}"
        );
    }
    let mut selected_missing = row.clone();
    let only_later = finalized_raw_inputs(&mut selected_missing, &[2]);
    assert_eq!(
        selected_missing
            .verify_finalized_raw_input_bytes(
                &"a".repeat(40),
                "producer-start",
                &only_later,
                Some((&plan, &cells, &tests))
            )
            .unwrap_err(),
        "verified selected attempt is absent from the raw input census"
    );
    let mut empty = row.clone();
    let empty_inputs = finalized_raw_inputs(&mut empty, &[]);
    assert_eq!(
        empty
            .verify_finalized_raw_input_bytes(
                &"a".repeat(40),
                "producer-start",
                &empty_inputs,
                Some((&plan, &cells, &tests))
            )
            .unwrap_err(),
        "current raw cells differ from the verified recorded artifact population"
    );
}

#[test]
fn finalized_raw_verifier_requires_full_zero_selection_not_merely_an_empty_census() {
    let (row, plan, _, test_bytes) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let mut plan: ConstructedValidationPlanV10 = serde_json::from_slice(&plan).unwrap();
    let cfg = dag_from_json(r#"{"steps":[{"group":"test","job":"fixture","cmd":"true","result_manifests":[{"kind":"structured-test-results","schema":2,"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.fixture"}]}]}"#).unwrap();
    plan.dag_json = dag_to_json(&cfg);
    plan.expected_e2e_plan_json = r#"{"schema":1,"cells":[]}"#.into();
    let plan = serde_json::to_vec(&plan).unwrap();
    let mut raw = serde_json::to_value(row).unwrap();
    raw["constructed_plan"]["sha256"] = hex_digest(&plan).into();
    raw["constructed_plan"]["bytes"] = (plan.len() as u64).into();
    let cells = &mut raw["cell_results"];
    for field in ["selected", "selected_backend_parity", "cells"] {
        cells[field] = serde_json::json!([]);
    }
    cells["selected_count"] = 0.into();
    cells["recorded_count"] = 0.into();
    cells["population_sha256"] = hex_digest(b"[]").into();
    cells["artifact"]["row_count"] = 0.into();
    cells["artifact"]["sha256"] = hex_digest(b"").into();
    // Zero selected cells still requires complete, nonempty test-producer
    // evidence under the existing schema-9 verifier. Do not invent an empty
    // test-population exception for this control.
    let selected = TestResultsSelectedPopulation {
        nodes: vec!["test.fixture".into()],
        compatibility: false,
    };
    let mut test_row: TestResultArtifactRow = serde_json::from_slice(&test_bytes).unwrap();
    test_row.producer = TestResultProducer::Node {
        node: "test.fixture".into(),
        outer_attempt: 1,
    };
    let mut test_bytes = serde_json::to_vec(&test_row).unwrap();
    test_bytes.push(b'\n');
    let tests = &mut raw["test_results"];
    tests["selected"] = serde_json::to_value(&selected).unwrap();
    tests["population_sha256"] = hex_digest(&serde_json::to_vec(&selected).unwrap()).into();
    tests["nodes"][0]["node"] = "test.fixture".into();
    tests["artifact"]["sha256"] = hex_digest(&test_bytes).into();
    let mut row: HistoryRow = serde_json::from_value(raw).unwrap();
    let inputs = finalized_raw_inputs(&mut row, &[]);
    assert!(
        row.verify_finalized_raw_input_bytes(
            &"a".repeat(40),
            "producer-start",
            &inputs,
            Some((&plan, b"", &test_bytes))
        )
        .unwrap()
    );
    assert_eq!(inputs.len(), 2, "two empty files remain distinct inputs");
    let mut missing = row.clone();
    missing.test_results = None;
    assert_eq!(
        missing
            .verify_finalized_raw_input_bytes(
                &"a".repeat(40),
                "producer-start",
                &inputs,
                Some((&plan, b"", &test_bytes))
            )
            .unwrap_err(),
        "schema 10 row omitted test_results"
    );
    let (selected_row, selected_plan, _, selected_tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let mut selected_row = serde_json::to_value(selected_row).unwrap();
    // Keep the genuine nonzero plan/selection while recording no cell rows.
    // Full test evidence is present, so this reaches the selected-zero guard.
    selected_row["cell_results"]["cells"] = serde_json::json!([]);
    selected_row["cell_results"]["recorded_count"] = 0.into();
    selected_row["cell_results"]["artifact"]["row_count"] = 0.into();
    selected_row["cell_results"]["artifact"]["sha256"] = hex_digest(b"").into();
    let mut selected_row: HistoryRow = serde_json::from_value(selected_row).unwrap();
    let empty_inputs = finalized_raw_inputs(&mut selected_row, &[]);
    let partial = selected_row
        .verify_schema10_artifact_bytes(&selected_plan, b"", &selected_tests)
        .unwrap()
        .unwrap();
    assert_eq!(partial.cell_results.selected_count, 1);
    assert_eq!(partial.missing_cells.len(), 1);
    assert!(partial.full_test_results);
    assert_eq!(
        selected_row
            .verify_finalized_raw_input_bytes(
                &"a".repeat(40),
                "producer-start",
                &empty_inputs,
                Some((&selected_plan, b"", &selected_tests))
            )
            .unwrap_err(),
        "empty current results contradict the finalized selected population"
    );
    let mut plan_with_missing_work: ConstructedValidationPlanV10 =
        serde_json::from_slice(&plan).unwrap();
    let cfg = dag_from_json(r#"{"steps":[{"group":"test","job":"fixture","cmd":"true","result_manifests":[{"kind":"structured-test-results","schema":2,"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.fixture"}]},{"group":"test","job":"omitted","cmd":"true","result_manifests":[{"kind":"structured-test-results","schema":2,"path_env":"DAGRUN_TEST_COUNTS_PATH","owner":"test.omitted"}]}]}"#).unwrap();
    plan_with_missing_work.dag_json = dag_to_json(&cfg);
    let bytes = serde_json::to_vec(&plan_with_missing_work).unwrap();
    let mut missing_work = serde_json::to_value(&row).unwrap();
    missing_work["constructed_plan"]["sha256"] = hex_digest(&bytes).into();
    missing_work["constructed_plan"]["bytes"] = (bytes.len() as u64).into();
    let missing_work: HistoryRow = serde_json::from_value(missing_work).unwrap();
    assert!(
        missing_work
            .verify_finalized_raw_input_bytes(
                &"a".repeat(40),
                "producer-start",
                &inputs,
                Some((&bytes, b"", &test_bytes))
            )
            .is_err(),
        "missing selected test work cannot become zero selection"
    );
}

/// One ordinary (non-parity) verify cell with `verdict`, through the same
/// artifact, summary and plan shapes the producer writes.
fn ordinary_fixture(verdict: CellVerdict) -> (HistoryRow, Vec<u8>, Vec<u8>, Vec<u8>) {
    let (row, plan, cells, tests) =
        fixture(parity(vec![completed(1, BackendParityVerdict::Matched)]));
    let mut plan: ConstructedValidationPlanV10 = serde_json::from_slice(&plan).unwrap();
    let mut cfg = plan.constructed_dag().unwrap();
    cfg.steps[0].cmd = crate::backend_parity_policy::HOSTED_ORDINARY_COMMAND.into();
    plan.dag_json = dag_to_json(&cfg);
    let plan = serde_json::to_vec(&plan).unwrap();
    let mut cell: CellArtifactResultV10 = {
        let mut value: Value = serde_json::from_slice(&cells).unwrap();
        for field in ["run_id", "hermit_sha", "source_tree_dirty"] {
            value.as_object_mut().unwrap().remove(field);
        }
        serde_json::from_value(value).unwrap()
    };
    cell.backend_parity = RequiredNullable::Null;
    cell.cell_verdict = verdict;
    let mut cell_row = serde_json::to_value(&cell).unwrap();
    cell_row["run_id"] = Value::String("fixture-v10".into());
    cell_row["hermit_sha"] = Value::String("a".repeat(40));
    cell_row["source_tree_dirty"] = Value::Bool(false);
    let mut cell_bytes = serde_json::to_vec(&cell_row).unwrap();
    cell_bytes.push(b'\n');
    let mut row = serde_json::to_value(&row).unwrap();
    row["cell_results"]["selected_backend_parity"] = serde_json::json!([]);
    row["cell_results"]["cells"][0] =
        serde_json::to_value(cell.summary("fixture-v10", &"a".repeat(40)).unwrap()).unwrap();
    row["cell_results"]["artifact"]["sha256"] = hex_digest(&cell_bytes).into();
    row["constructed_plan"]["sha256"] = hex_digest(&plan).into();
    row["constructed_plan"]["bytes"] = (plan.len() as u64).into();
    (
        serde_json::from_value(row).unwrap(),
        plan,
        cell_bytes,
        tests,
    )
}

/// The verdict the ledger derives for a declared stripped verify cell from
/// the real stripped report Hermit wrote, matched or diverged.
fn stripped_verdict(diverged: bool) -> CellVerdict {
    let report = if diverged {
        crate::runner::PRODUCER_STRIPPED_REPORT
            .replace(r#""verified":true"#, r#""verified":false"#)
            .replace(r#""verdict":"matched""#, r#""verdict":"diverged""#)
    } else {
        crate::runner::PRODUCER_STRIPPED_REPORT.to_string()
    };
    super::super::cell_verdict_from_source(&serde_json::json!({
        "mode": "verify",
        "outcome": if diverged { "FAIL" } else { "PASS" },
        "relaxations": ["comparator=stripped: the corpus verdict policy is the stripped comparison"],
        "attempts": [{
            "verification_report_sha256": hex_digest(report.as_bytes()),
            "verification_report": report,
        }],
    }))
    .unwrap()
}

#[test]
fn a_declared_stripped_cell_is_retained_and_verified_as_weak_ordinary_evidence() {
    for (diverged, expected) in [
        (false, ComparisonObservationVerdictV10::Matched),
        (true, ComparisonObservationVerdictV10::Diverged),
    ] {
        let verdict = stripped_verdict(diverged);
        assert!(
            matches!(
                &verdict,
                CellVerdict::ComparedAndMatched {
                    comparison_tier: ComparisonTier::ExitAndStreamEquality,
                    bitwise_parity: false,
                    ..
                } | CellVerdict::ComparedAndDiverged {
                    comparison_tier: ComparisonTier::ExitAndStreamEquality,
                    bitwise_parity: false,
                    ..
                }
            ),
            "{verdict:?}"
        );
        let (row, plan, cells, tests) = ordinary_fixture(verdict);
        let verified = row
            .verify_schema10_artifact_bytes(&plan, &cells, &tests)
            .unwrap()
            .unwrap();
        assert!(verified.missing_cells.is_empty() && verified.full_test_results);
        assert_eq!(verified.observations.len(), 1);
        assert_eq!(
            verified.observations[0].relation,
            ComparisonRelationV10::Ordinary
        );
        assert_eq!(verified.observations[0].verdict, expected);
    }
}

#[test]
fn the_weak_tier_is_refused_unless_it_is_exactly_a_stripped_verify_comparison() {
    let identity = identity();
    let CellVerdict::ComparedAndMatched {
        comparison,
        compared_log_messages,
        ..
    } = stripped_verdict(false)
    else {
        panic!("expected a stripped match");
    };
    let weak =
        |comparison: &ComparisonSpec, bitwise_parity: bool| CellVerdict::ComparedAndMatched {
            comparison_tier: ComparisonTier::ExitAndStreamEquality,
            comparison: comparison.clone(),
            bitwise_parity,
            compared_log_messages: compared_log_messages.clone(),
        };
    validate_ordinary_verdict(&identity, &weak(&comparison, false)).unwrap();
    let refused = |identity: &CellIdentity, verdict: &CellVerdict| {
        assert!(
            validate_ordinary_verdict(identity, verdict)
                .unwrap_err()
                .contains("contradicts its canonical comparison"),
            "{verdict:?}"
        );
    };
    // Claiming bitwise parity, or the canonical tier, on a stripped comparison.
    refused(&identity, &weak(&comparison, true));
    refused(
        &identity,
        &CellVerdict::ComparedAndMatched {
            comparison_tier: ComparisonTier::CanonicalBitwise,
            comparison: comparison.clone(),
            bitwise_parity: true,
            compared_log_messages: compared_log_messages.clone(),
        },
    );
    // Any weaker stripped evidence.
    for change in [
        |c: &mut ComparisonSpec| c.compare_logs = false,
        |c: &mut ComparisonSpec| c.virtualize_time = Some(false),
        |c: &mut ComparisonSpec| c.virtualize_time = None,
        |c: &mut ComparisonSpec| c.strictness = ComparisonStrictness::Canonical,
    ] {
        let mut weaker = comparison.clone();
        change(&mut weaker);
        refused(&identity, &weak(&weaker, false));
    }
    let empty = CellVerdict::ComparedAndMatched {
        comparison_tier: ComparisonTier::ExitAndStreamEquality,
        comparison: comparison.clone(),
        bitwise_parity: false,
        compared_log_messages: RequiredNullable::Value(ComparedLogCounts { left: 0, right: 7 }),
    };
    refused(&identity, &empty);
    // The stripped comparator exists only in verify mode.
    for mode in ["replay", "chaos"] {
        let other = CellIdentity {
            mode: mode.into(),
            ..identity.clone()
        };
        refused(&other, &weak(&comparison, false));
    }
}
