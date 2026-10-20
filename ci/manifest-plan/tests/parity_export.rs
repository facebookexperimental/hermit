/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! `test-harness parity export`, run as a subprocess: the rows it prints are
//! the ones `series.py append-parity` publishes for a run, so its call site,
//! its refusal and its exit status are what is pinned here. The row rules
//! themselves are unit-tested beside `parity::ledger_sources`.
//! <https://github.com/rrnewton/hermit/issues/3301>

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

use serde_json::Value;
use serde_json::json;

const SHA: &str = "d3a0a4ae3d168565595ae4157b25ab8efbb1861a";
const RUN: &str = "validate-coord-s17-d3a0a4ae3d16-1790649970094317174-3328074-da07a1da";

/// One record copied byte-for-byte from RUN 1953's
/// `e2e/portable/manifest_backend_parity_c/parity.jsonl` (its first line):
/// the kvm `brk(NULL)` divergence at record 13, with the `unavailable_class`
/// and `operand` every record now carries (both `null` for a measured
/// verdict) inserted after its verdict.
const REAL_RECORD: &str = r#"{"schema":1,"test_id":"backend-parity-c/aio-refusal","backend":"kvm","verdict":"diverged","unavailable_class":null,"operand":null,"inputs_equalized":false,"reason":null,"credit":null,"unequalized_credit":0.11320754716981132,"first_divergent_record":13,"left_len":106,"right_len":106,"matched_prefix":12,"first_difference":{"field":"token 12: `Ok(93824992251904)` vs `Ok(2117632)`","syscall":2,"scheduler_turn":1,"virtual_nanoseconds":1790651878158833000,"reference_message":"INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #<NUM>: brk(NULL) = Ok(93824992251904)","candidate_message":"INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #<NUM>: brk(NULL) = Ok(2117632)"},"reference_log":"/results/portable/manifest_backend_parity_c/parity/golden/backend-parity-c/aio-refusal.detlog","candidate_log":"/results/runs/validate-coord-s17-d3a0a4ae3d16-1790649970094317174-3328074-da07a1da/backend-parity-c-aio-refusal-verify-kvm/verify-logs/verify-1/run1_log_2p5r0","run_id":"validate-coord-s17-d3a0a4ae3d16-1790649970094317174-3328074-da07a1da","hermit_sha":"d3a0a4ae3d168565595ae4157b25ab8efbb1861a"}"#;

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hermit-parity-export-{}-{label}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn export(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_test-harness"))
        .arg("parity")
        .arg("export")
        .args(args)
        .output()
        .expect("run test-harness parity export")
}

fn status(state: &str, scope: Option<&[&str]>, cells: usize, error: Option<&str>) -> Value {
    let mut status = json!({
        "schema": if scope.is_some() { 2 } else { 1 },
        "state": state,
        "run_id": RUN,
        "hermit_sha": SHA,
        "hermit_bin": "/src/hermit",
        "hermit_bin_sha256": "a284a7e012a2d993cfc121e0cdb5c07daf3406d93eb85469801ed800c29eaad3",
        "records": "/results/parity.jsonl",
        "cells": cells,
        "summary": null,
        "error": error,
    });
    if let Some(scope) = scope {
        status["scope"] = json!(scope);
    }
    status
}

fn write_node(node: &Path, status: Option<&Value>, records: Option<&str>) {
    fs::create_dir_all(node).unwrap();
    if let Some(status) = status {
        fs::write(
            node.join("parity.status.json"),
            serde_json::to_vec_pretty(status).unwrap(),
        )
        .unwrap();
    }
    if let Some(records) = records {
        fs::write(node.join("parity.jsonl"), records).unwrap();
    }
}

fn rows(output: &Output) -> Vec<Value> {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// `record` with each `(from, to)` replaced, where `from` occurs exactly
/// once, so a fixture edit can neither miss its field nor touch another.
fn edited(record: &str, edits: &[(&str, &str)]) -> String {
    let mut record = record.to_string();
    for &(from, to) in edits {
        assert_eq!(record.matches(from).count(), 1, "{from} in {record}");
        record = record.replace(from, to);
    }
    record
}

/// The whole `"first_difference":{...}` member of `record`, an object with
/// no object inside it.
fn first_difference(record: &str) -> &str {
    let start = record.find(r#""first_difference":{"#).unwrap();
    let end = start + record[start..].find('}').unwrap() + 1;
    assert!(
        record[end..].starts_with(r#","reference_log":"#),
        "{record}"
    );
    &record[start..end]
}

#[test]
fn export_prints_one_row_per_owed_cell_and_passes_real_records_through() {
    let root = scratch("rows");
    write_node(
        &root.join("portable/manifest_c_programs"),
        Some(&status("complete", None, 1, None)),
        Some(&format!("{REAL_RECORD}\n")),
    );
    write_node(
        &root.join("privileged/manifest_c_programs"),
        Some(&status(
            "failed",
            Some(&[
                "c-programs/cpuid-probe@kvm",
                "c-programs/cpuid-probe@liteinst",
            ]),
            2,
            Some("log-diff timed out"),
        )),
        None,
    );
    let scope = root.join("expected.json");
    fs::write(
        &scope,
        r#"{"portable/manifest_system_utils": ["system-utils/ls@sabre"]}"#,
    )
    .unwrap();
    let output = export(&[
        "--e2e-root",
        root.to_str().unwrap(),
        "--expected-scope",
        scope.to_str().unwrap(),
    ]);
    let rows = rows(&output);
    let summary = rows
        .iter()
        .map(|row| {
            (
                row["cell"].as_str().unwrap(),
                row["verdict"].as_str().unwrap(),
                row["source"]["post_pass_state"].as_str().unwrap(),
                row["source"]["scope_source"].as_str().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            (
                "backend-parity-c/aio-refusal@kvm",
                "diverged",
                "complete",
                "status-count"
            ),
            (
                "system-utils/ls@sabre",
                "record-missing",
                "absent",
                "expected-scope"
            ),
            (
                "c-programs/cpuid-probe@kvm",
                "record-missing",
                "failed",
                "status-scope"
            ),
            (
                "c-programs/cpuid-probe@liteinst",
                "record-missing",
                "failed",
                "status-scope"
            ),
        ]
    );
    // The record reaches the ledger byte-for-byte as the post-pass wrote it:
    // the same fields in the same order, so the raw row text contains it.
    let first_line = String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(
        first_line.contains(&format!(r#""record":{REAL_RECORD}"#)),
        "{first_line}"
    );
    assert_eq!(rows[0]["run_id"], RUN);
    assert_eq!(rows[0]["hermit_sha"], SHA);
    assert_eq!(
        rows[2]["reason"],
        "parity post-pass failed: log-diff timed out"
    );
    assert_eq!(
        rows[1]["reason"],
        "no parity row: node portable/manifest_system_utils left no parity.status.json"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("parity export: 4 source row(s) from"),
        "{stderr}"
    );
}

#[test]
fn export_refuses_an_inconsistent_node_and_prints_no_rows() {
    let root = scratch("refused");
    write_node(
        &root.join("portable/manifest_c_programs"),
        Some(&status("complete", None, 1, None)),
        Some(&format!("{REAL_RECORD}\n")),
    );
    write_node(
        &root.join("portable/manifest_system_utils"),
        None,
        Some(&format!("{REAL_RECORD}\n")),
    );
    let output = export(&["--e2e-root", root.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("has no parity.status.json beside it"),
        "{stderr}"
    );

    // A record carrying clean credit beside its unequalized credit is refused
    // by the check that keeps the two apart, which comes before any check of
    // the verdict.
    fs::remove_dir_all(root.join("portable/manifest_system_utils")).unwrap();
    let both_credits = edited(
        REAL_RECORD,
        &[
            (r#""verdict":"diverged""#, r#""verdict":"matched""#),
            (r#""credit":null"#, r#""credit":0.5"#),
        ],
    );
    write_node(
        &root.join("portable/manifest_c_programs"),
        None,
        Some(&format!("{both_credits}\n")),
    );
    let output = export(&["--e2e-root", root.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(
            "parity record backend-parity-c/aio-refusal@kvm: credit and unequalized_credit are \
             exclusive"
        ),
        "{stderr}"
    );

    // A matched record with partial credit breaks a credit invariant. It
    // passes every earlier check: equal inputs with clean credit only, no
    // divergence position, and a credit that agrees with its prefix (53 of
    // 106 is 0.5).
    let partial_match = edited(
        REAL_RECORD,
        &[
            (r#""verdict":"diverged""#, r#""verdict":"matched""#),
            (r#""inputs_equalized":false"#, r#""inputs_equalized":true"#),
            (r#""credit":null"#, r#""credit":0.5"#),
            (
                r#""unequalized_credit":0.11320754716981132"#,
                r#""unequalized_credit":null"#,
            ),
            (
                r#""first_divergent_record":13"#,
                r#""first_divergent_record":null"#,
            ),
            (r#""matched_prefix":12"#, r#""matched_prefix":53"#),
            (first_difference(REAL_RECORD), r#""first_difference":null"#),
        ],
    );
    write_node(
        &root.join("portable/manifest_c_programs"),
        None,
        Some(&format!("{partial_match}\n")),
    );
    let output = export(&["--e2e-root", root.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(
            "parity record backend-parity-c/aio-refusal@kvm: a match must be full credit, got \
             Some(0.5)"
        ),
        "{stderr}"
    );
    // The same record at full credit is published, so the partial credit
    // alone is what was refused.
    let full_match = edited(
        &partial_match,
        &[
            (r#""credit":0.5"#, r#""credit":1.0"#),
            (r#""matched_prefix":53"#, r#""matched_prefix":106"#),
        ],
    );
    write_node(
        &root.join("portable/manifest_c_programs"),
        None,
        Some(&format!("{full_match}\n")),
    );
    let published = rows(&export(&["--e2e-root", root.to_str().unwrap()]));
    assert_eq!(published.len(), 1, "{published:?}");
    assert_eq!(published[0]["verdict"], "matched", "{published:?}");

    // A record written before every record carried its typed class is
    // refused, not read without one.
    let untyped = REAL_RECORD.replace(r#""unavailable_class":null,"operand":null,"#, "");
    write_node(
        &root.join("portable/manifest_c_programs"),
        None,
        Some(&format!("{untyped}\n")),
    );
    let output = export(&["--e2e-root", root.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("missing field `unavailable_class`"),
        "{stderr}"
    );

    let output = export(&[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("parity export requires --e2e-root <DIR>")
    );
}
