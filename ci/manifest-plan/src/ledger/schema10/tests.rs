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
            "/home/fixture/hermit".into(),
            "run".into(),
            "--backend".into(),
            backend.into(),
            "--strict".into(),
            "--verify".into(),
            "--verify-strict".into(),
            "--".into(),
            "/home/fixture/guest".into(),
        ],
        guest_argv: vec!["/home/fixture/guest".into()],
        env: BTreeMap::new(),
        cwd: "/home/fixture/work".into(),
        shell_command: "/home/fixture/hermit run a synthetic fixture".into(),
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
        path: ValidatePath::Full,
        compatibility_selected: false,
        dag_json: dag_to_json(&cfg),
        expected_e2e_plan_json: serde_json::json!({"schema":1,"cells":[id]}).to_string(),
    };
    let plan_bytes = serde_json::to_vec(&plan).unwrap();
    let cell = CellArtifactResultV10 {
        lane: id.lane.clone(),
        category: id.category.clone(),
        test: id.test.clone(),
        mode: id.mode.clone(),
        backend: id.backend.clone(),
        cell_verdict: parity.candidate_verdict(&id).unwrap(),
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
        path: ValidatePath::Full,
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
        cells: vec![cell.summary().unwrap()],
    };
    let test_row = TestResultArtifactRow {
        run_id: run_id.clone(),
        hermit_sha: hermit_sha.clone(),
        path: ValidatePath::Full,
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
    };
    let tests = TestResultsEvidenceV9 {
        path: ValidatePath::Full,
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
    let row = serde_json::from_value(serde_json::json!({"schema_version":10,"run_id":run_id,"commit":hermit_sha,"profile":"full","tree_dirty":false,
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
    assert!(!compact.contains("/home/fixture"));
    assert!(
        String::from_utf8(cells.clone())
            .unwrap()
            .contains("/home/fixture")
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
    reference.0.reason = Some("/home/fixture/reference exited before JSON".into());
    reference.0.verification_report = None;
    reference.0.verification_report_sha256 = None;
    let unavailable = BackendParityCellAttempt::UnavailableWithReason {
        attempt: 1,
        candidate_attempt: complete.candidate_attempt().clone(),
        reference_attempt: RequiredNullable::Value(reference.clone()),
        reason: "/home/fixture/reference did not finish".into(),
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

// Use the real generated graph and label selection, including the pinned-root
// wrapper additions. The small report fixture above intentionally remains a
// synthetic single-cell input; it does not cover generated command bytes.
fn generated_plan_populations_preserve_command_policy() {
    let root = crate::validation_dag::repo_root().unwrap();
    let generated = crate::validation_dag::generate(&root).unwrap();
    assert_eq!(
        crate::validation_dag::canonical_text(&generated),
        std::fs::read_to_string(root.join("ci/dag/validate.json")).unwrap()
    );
    let expected_json = std::fs::read_to_string(root.join("ci/expected-e2e-plan.json")).unwrap();
    let expected_cells = crate::validation_dag::expected_cells_from_json(&expected_json)
        .unwrap()
        .iter()
        .map(exact_identity)
        .collect::<Result<BTreeSet<_>, _>>()
        .unwrap();
    assert_eq!(expected_cells.len(), 756);
    for (label, tag, cell_count) in [
        ("full", "e2e.manifest_backend_parity_c", 756),
        (
            "hosted-portable",
            "e2e.manifest_backend_parity_c_on_host",
            753,
        ),
    ] {
        let selected = dagrun::select_steps_by_labels(&generated, &[label.to_owned()]).unwrap();
        let expected_selected = expected_cells
            .iter()
            .filter(|cell| label == "full" || cell.lane == "portable")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(expected_selected.len(), cell_count);
        for active in [false, true] {
            let mut cfg = selected.clone();
            let index = cfg.steps.iter().position(|step| step.tag() == tag).unwrap();
            let command = &mut cfg.steps[index].cmd;
            assert!(command.matches("--parity-reference ptrace ").count() <= 1);
            *command = command.replace("--parity-reference ptrace ", "");
            assert_eq!(command.matches("--prebuilt --jobs 8").count(), 1);
            if active {
                *command = command.replace(
                    "--prebuilt --jobs 8",
                    "--prebuilt --parity-reference ptrace --jobs 8",
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
                expected_e2e_plan_json: expected_json.clone(),
            };
            assert_eq!(plan.planned_cells().unwrap(), expected_selected);
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
            assert_eq!(expected_relations.len(), if active { 97 } else { 0 });
            assert_eq!(
                plan.planned_backend_parity_relations().unwrap(),
                expected_relations
            );
            if active {
                for (backend, count) in [("kvm", 75), ("liteinst", 21), ("sabre", 1)] {
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
            ] {
                if (mutation == "unknown-node" && !active)
                    || (label != "full"
                        && matches!(
                            mutation,
                            "raw-command" | "legacy-guard" | "missing-env" | "duplicate-env"
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
        }
    }
}
