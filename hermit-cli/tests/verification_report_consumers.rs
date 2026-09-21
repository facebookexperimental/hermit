/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;

struct Consumer {
    path: &'static str,
    requirement: &'static str,
    invocation: &'static str,
    minimum_invocations: usize,
}

const CONSUMERS: &[Consumer] = &[
    Consumer {
        path: "tests/backend-parity/e9patch_corpus.py",
        requirement: "matched",
        invocation: "verification_matched(hermit,",
        minimum_invocations: 2,
    },
    Consumer {
        path: "tests/backend-parity/run_matrix.py",
        requirement: "matched",
        invocation: "[str(VERIFICATION_REPORT_BIN), \"--json\", \"matched\", str(path)]",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/data-handling/common.bash",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/determinism-stress/common.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/language-runtimes/run.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/e2e/lib/system-utils/_common.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$VERIFY_REPORT\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_network_test.sh",
        requirement: "canonical-match",
        invocation: "\"$VERIFICATION_REPORT_BIN\" canonical-match \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_test.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/qemu-boot/strict_l2_userspace_test.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/standalone/strict_setitimer.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
    Consumer {
        path: "tests/standalone/strict_timer_create.sh",
        requirement: "matched",
        invocation: "\"$VERIFICATION_REPORT_BIN\" matched \"$verify_report\"",
        minimum_invocations: 1,
    },
];

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn temporary_directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hermit-verification-report-consumers-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("create temporary directory");
    path
}

const ARTIFACT_CONSUMER_CHILD: &str = "HERMIT_ARTIFACT_CONSUMER_CHILD";
const ARTIFACT_CONSUMER_ENTRY_MARKER: &str = "HERMIT_ARTIFACT_CONSUMER_ENTRY_MARKER";
const ARTIFACT_CONSUMER_MODE: &str = "artifact";
const STANDALONE_CARGO_MODE: &str = "standalone-cargo";

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("make {} executable: {error}", path.display()));
}

fn run_artifact_consumer(root: &Path, pointer: &Path, marker: &Path) -> Output {
    Command::new(root.join("ci/run-with-hermit-e2e-artifact.sh"))
        .env("HERMIT_E2E_ARTIFACT_POINTER", pointer)
        .env(ARTIFACT_CONSUMER_CHILD, ARTIFACT_CONSUMER_MODE)
        .env(ARTIFACT_CONSUMER_ENTRY_MARKER, marker)
        .arg(std::env::current_exe().expect("locate the running integration-test binary"))
        .args([
            "--exact",
            "immutable_artifact_controls_a_real_integration_consumer",
            "--nocapture",
        ])
        .output()
        .expect("run the integration-test consumer through the artifact wrapper")
}

fn copy_binary_only_bundle(source: &Path, destination: &Path) {
    fs::create_dir_all(destination)
        .unwrap_or_else(|error| panic!("create {}: {error}", destination.display()));
    for name in ["hermit", "hermit.sha256", "kind"] {
        fs::copy(source.join(name), destination.join(name)).unwrap_or_else(|error| {
            panic!("copy {} into {}: {error}", name, destination.display())
        });
    }
}

#[test]
fn immutable_artifact_controls_a_real_integration_consumer() {
    match std::env::var(ARTIFACT_CONSUMER_CHILD).as_deref() {
        Ok(ARTIFACT_CONSUMER_MODE) => {
            let marker = std::env::var_os(ARTIFACT_CONSUMER_ENTRY_MARKER)
                .map(PathBuf::from)
                .expect("child invocation lacks its entry marker");
            fs::write(&marker, b"entered\n")
                .unwrap_or_else(|error| panic!("write {}: {error}", marker.display()));
            let output = Command::new(hermit_test::hermit_binary())
                .output()
                .expect("execute the Hermit binary selected by the shared test resolver");
            assert!(
                output.status.success(),
                "selected Hermit failed: {output:?}"
            );
            assert_eq!(output.stdout, b"expected-identity\n");
            return;
        }
        Ok(STANDALONE_CARGO_MODE) => {
            assert!(
                std::env::var_os("HERMIT_BIN").is_none(),
                "standalone Cargo child unexpectedly inherited HERMIT_BIN"
            );
            assert_eq!(
                hermit_test::hermit_binary(),
                Path::new(env!("CARGO_BIN_EXE_hermit")),
                "standalone Cargo did not select its compile-time Hermit binary"
            );
            let output = Command::new(hermit_test::hermit_binary())
                .arg("--version")
                .output()
                .expect("execute standalone Cargo's compiled Hermit binary");
            assert!(
                output.status.success(),
                "standalone Cargo's compiled Hermit --version failed: {output:?}"
            );
            return;
        }
        Ok(mode) => panic!("unknown artifact consumer child mode {mode:?}"),
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("artifact consumer child mode is not valid UTF-8")
        }
        Err(std::env::VarError::NotPresent) => {}
    }

    let root = root();
    if std::env::var_os("HERMIT_BIN").is_none_or(|path| path.is_empty()) {
        let standalone = Command::new(
            std::env::current_exe().expect("locate the running standalone integration-test binary"),
        )
        .env_remove("HERMIT_BIN")
        .env(ARTIFACT_CONSUMER_CHILD, STANDALONE_CARGO_MODE)
        .args([
            "--exact",
            "immutable_artifact_controls_a_real_integration_consumer",
            "--nocapture",
        ])
        .output()
        .expect("self-spawn the standalone Cargo integration-test consumer");
        assert!(
            standalone.status.success(),
            "standalone Cargo integration consumer failed:\n{}",
            String::from_utf8_lossy(&standalone.stderr)
        );
    }

    let temporary = temporary_directory();
    let mutable = temporary.join("target/debug/hermit");
    let bundles = temporary.join("target/ci/hermit-e2e-artifacts");
    let pointer = temporary.join("target/ci/hermit-e2e-artifact.path");
    fs::create_dir_all(mutable.parent().expect("mutable binary has a parent"))
        .expect("create mutable target directory");
    write_executable(&mutable, "#!/bin/sh\nprintf 'expected-identity\\n'\n");

    let published = Command::new(root.join("ci/publish-hermit-e2e-artifact.sh"))
        .args([mutable.as_path(), bundles.as_path(), pointer.as_path()])
        .output()
        .expect("publish the fixture Hermit artifact");
    assert!(
        published.status.success(),
        "fixture artifact publication failed:\n{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let bundle = PathBuf::from(
        fs::read_to_string(&pointer)
            .expect("read fixture artifact pointer")
            .trim(),
    );

    let replacement = mutable.with_extension("next");
    write_executable(&replacement, "#!/bin/sh\nprintf 'wrong-relinked\\n'\n");
    fs::rename(&replacement, &mutable).expect("atomically relink mutable Hermit source");
    let relink_marker = temporary.join("relink-consumer-entered");
    let relink = run_artifact_consumer(&root, &pointer, &relink_marker);
    assert!(
        relink.status.success(),
        "real integration consumer followed the relinked mutable path:\n{}",
        String::from_utf8_lossy(&relink.stderr)
    );
    assert!(
        relink_marker.is_file(),
        "real integration consumer was never entered for the valid immutable artifact"
    );

    for (state, expected_reason) in [
        (
            "absent",
            "published Hermit is missing, empty, or non-executable",
        ),
        (
            "nonexec",
            "published Hermit is missing, empty, or non-executable",
        ),
        ("wrong-hash", "published Hermit hash mismatch"),
    ] {
        let fake_bundle = temporary.join(state).join(
            bundle
                .file_name()
                .expect("published bundle has an identity"),
        );
        copy_binary_only_bundle(&bundle, &fake_bundle);
        match state {
            "absent" => fs::remove_file(fake_bundle.join("hermit"))
                .expect("remove the fake artifact binary"),
            "nonexec" => fs::set_permissions(
                fake_bundle.join("hermit"),
                fs::Permissions::from_mode(0o644),
            )
            .expect("make the fake artifact binary non-executable"),
            "wrong-hash" => OpenOptions::new()
                .append(true)
                .open(fake_bundle.join("hermit"))
                .and_then(|mut file| file.write_all(b"corruption"))
                .expect("corrupt the fake artifact binary"),
            _ => unreachable!(),
        }
        let fake_pointer = temporary.join(format!("{state}.path"));
        fs::write(&fake_pointer, format!("{}\n", fake_bundle.display()))
            .expect("write fake artifact pointer");
        let marker = temporary.join(format!("{state}-consumer-entered"));
        let refused = run_artifact_consumer(&root, &fake_pointer, &marker);
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{state} artifact returned the wrong status:\n{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains(expected_reason),
            "{state} artifact refusal did not name {expected_reason:?}:\n{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            !marker.exists(),
            "{state} artifact entered the protected integration consumer"
        );
    }

    fs::remove_dir_all(temporary).expect("remove artifact-contract temporary directory");
}

fn measured_match() -> Value {
    // Exact evidence values from the measured ptrace report recorded in
    // ci/compat-envelope/cells.json for e69c0a62cecef9aa44e3810ae88c06ad24155048.
    serde_json::json!({
        "verified": true,
        "bitwise_parity": true,
        "verdict": "matched",
        "no_result_reason": null,
        "infrastructure_error": null,
        "comparison": {
            "strictness": "canonical",
            "display_name": "BitwiseInfoV1",
            "compare_logs": true,
            "compare_io_buffers": true,
            "log_scope": "info",
            "record_envelope": "all_records_v1",
            "virtualize_time": true,
            "strip_lines": false,
            "canonicalize_addresses": false,
            "full_trace": false,
            "exact_remainder": true,
            "stripped_prefixes": [],
            "canonicalizations": [],
            "ignore_lines": false,
            "skip_commit": false,
            "skip_detlog": false
        },
        "compared_log_messages": {"left": 266, "right": 266},
        "dbt_counted_branches": null,
        "runtime": null,
        "guest_exit_code": 0,
        "guest_signal": null,
        "first_divergent_scheduler_turn": null,
        "first_divergent_virtual_nanoseconds": null,
        "first_divergent_record": null,
        "first_divergent_syscall": null,
        "first_divergent_left_message": null,
        "first_divergent_right_message": null
    })
}

fn current_synthetic_match() -> Value {
    // Synthetic current report: the historical measured fixture above remains
    // unchanged. Both synthetic executions exit zero with empty byte streams.
    let mut report = measured_match();
    let output = serde_json::json!({"exit_code": 0, "signal": null,
        "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "stdout_bytes": 0,
        "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "stderr_bytes": 0});
    report["compared_outputs"] = serde_json::json!({"left": output, "right": output});
    report["comparison"]["canonicalize_addresses"] = serde_json::json!(true);
    report["comparison"]["full_trace"] = serde_json::json!(true);
    report["comparison"]["stripped_prefixes"] = serde_json::json!(["real-wall-clock-prefix/v1"]);
    report["comparison"]["canonicalizations"] =
        serde_json::json!(["host-address-to-first-appearance-ordinal/v1"]);
    report["compared_log_messages"] = serde_json::json!({"left": 2, "right": 2});
    report
}

fn write_report(path: &Path, report: &Value) {
    fs::write(path, format!("{report}\n")).expect("write verification report");
}

fn verdict(requirement: &str, path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_verification-report"))
        .arg(requirement)
        .arg(path)
        .output()
        .expect("run verification-report")
}

#[test]
fn every_named_consumer_delegates_to_the_shared_typed_reader() {
    let root = root();
    assert_eq!(CONSUMERS.len(), 11, "the published consumer list changed");
    for consumer in CONSUMERS {
        let source = fs::read_to_string(root.join(consumer.path))
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", consumer.path));
        assert!(
            source.contains("VERIFICATION_REPORT_BIN"),
            "{} does not name the shared typed reader",
            consumer.path
        );
        assert!(
            source.contains("--verify-json"),
            "{} does not request the producer-owned report",
            consumer.path
        );
        assert!(
            source.matches(consumer.invocation).count() >= consumer.minimum_invocations,
            "{} does not use the typed reader's {} result at every decision site",
            consumer.path,
            consumer.requirement
        );
        assert!(
            !source.lines().any(|line| {
                line.contains("Determinism verified")
                    && (line.contains("grep")
                        || line.contains(" in stderr")
                        || line.contains(" in result."))
            }),
            "{} still makes a functional decision from the banner",
            consumer.path
        );
    }
}

#[test]
fn current_match_and_a_typed_verdict_mutation_bracket_every_consumer() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");
    let historical = measured_match();
    assert!(historical.get("compared_outputs").is_none());
    let retained = hermit::canonical_verdict::VerificationReport::from_json_slice(
        &serde_json::to_vec(&historical).unwrap(),
    )
    .unwrap();
    assert!(retained.compared_outputs.is_none());
    write_report(&report_path, &historical);
    for consumer in CONSUMERS {
        let refused = verdict(consumer.requirement, &report_path);
        assert_eq!(refused.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("compared_outputs"));
    }
    let matched = current_synthetic_match();
    write_report(&report_path, &matched);

    for consumer in CONSUMERS {
        let accepted = verdict(consumer.requirement, &report_path);
        assert!(
            accepted.status.success(),
            "{} did not accept the current synthetic typed match: {}",
            consumer.path,
            String::from_utf8_lossy(&accepted.stderr)
        );
    }

    let mut diverged = matched;
    diverged["verified"] = serde_json::json!(false);
    diverged["bitwise_parity"] = serde_json::json!(false);
    diverged["verdict"] = serde_json::json!("diverged");
    write_report(&report_path, &diverged);

    for consumer in CONSUMERS {
        let refused = verdict(consumer.requirement, &report_path);
        assert_eq!(
            refused.status.code(),
            Some(1), // EXIT-CLASS: verification-report requirement unmet.
            "{} ignored the mutated typed verdict: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    let mut infrastructure_error = current_synthetic_match();
    infrastructure_error["verified"] = serde_json::json!(false);
    infrastructure_error["bitwise_parity"] = serde_json::json!(false);
    infrastructure_error["verdict"] = serde_json::json!("infrastructure_error");
    infrastructure_error["comparison"] = serde_json::Value::Null;
    infrastructure_error["compared_log_messages"] = serde_json::Value::Null;
    infrastructure_error["infrastructure_error"] =
        serde_json::json!({"kind": "skid_overshoot", "count": 2});
    write_report(&report_path, &infrastructure_error);

    for consumer in CONSUMERS {
        let refused = verdict(consumer.requirement, &report_path);
        assert_eq!(
            refused.status.code(),
            Some(1), // EXIT-CLASS: verification-report requirement unmet.
            "{} accepted a typed infrastructure error: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("infrastructure_error"),
            "{} refused a typed infrastructure error without naming it: {}",
            consumer.path,
            String::from_utf8_lossy(&refused.stderr)
        );
    }
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}

#[test]
fn current_shape_and_canonical_evidence_fail_by_name() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");

    let mut unknown = current_synthetic_match();
    unknown["verdict"] = serde_json::json!("future_verdict");
    write_report(&report_path, &unknown);
    let refused = verdict("matched", &report_path);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("unknown variant `future_verdict`"),
        "unknown verdict must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let mut incomplete = current_synthetic_match();
    incomplete
        .as_object_mut()
        .expect("object")
        .remove("guest_signal");
    write_report(&report_path, &incomplete);
    let refused = verdict("matched", &report_path);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("missing current producer field `guest_signal`"),
        "missing current field must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    let mut stripped = current_synthetic_match();
    stripped["bitwise_parity"] = serde_json::json!(false);
    stripped["comparison"]["strictness"] = serde_json::json!("stripped");
    write_report(&report_path, &stripped);
    let refused = verdict("canonical-match", &report_path);
    // EXIT-CLASS: verification-report canonical-match requirement unmet.
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("strictness=stripped"),
        "weakened comparison must fail by name: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}

#[test]
fn json_output_retains_failure_evidence_without_satisfying_a_match_requirement() {
    let temporary = temporary_directory();
    let report_path = temporary.join("verify.json");
    for name in ["matched", "diverged", "no_result", "infrastructure_error"] {
        let mut report = current_synthetic_match();
        report["verdict"] = serde_json::json!(name);
        if name != "matched" {
            report["verified"] = serde_json::json!(false);
            report["bitwise_parity"] = serde_json::json!(false);
        }
        if matches!(name, "no_result" | "infrastructure_error") {
            report["comparison"] = Value::Null;
            report["compared_log_messages"] = Value::Null;
            report["compared_outputs"] = Value::Null;
        }
        if name == "infrastructure_error" {
            report["infrastructure_error"] =
                serde_json::json!({"kind": "skid_overshoot", "count": 2});
        }
        write_report(&report_path, &report);
        for requirement in ["matched", "canonical-match"] {
            let output = Command::new(env!("CARGO_BIN_EXE_verification-report"))
                .args(["--json", requirement])
                .arg(&report_path)
                .output()
                .expect("read report with JSON output");
            assert_eq!(
                output.status.code(),
                Some(if name == "matched" { 0 } else { 1 }),
                "{name} must retain its {requirement} exit status: {output:?}"
            );
            let parsed: Value = serde_json::from_slice(&output.stdout).expect("typed JSON");
            for field in [
                "verified",
                "bitwise_parity",
                "verdict",
                "infrastructure_error",
                "comparison",
                "compared_log_messages",
                "guest_exit_code",
                "guest_signal",
            ] {
                assert_eq!(parsed[field], report[field], "{name}: {field}");
            }
            if name == "matched" {
                assert!(output.stderr.is_empty(), "{output:?}");
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr).contains(name),
                    "failure must still be named: {output:?}"
                );
            }
        }
    }

    for malformed in ["missing field", "unknown verdict", "invalid cause"] {
        let mut report = current_synthetic_match();
        match malformed {
            "missing field" => {
                report.as_object_mut().unwrap().remove("guest_signal");
            }
            "unknown verdict" => report["verdict"] = serde_json::json!("future_verdict"),
            "invalid cause" => {
                report["verified"] = serde_json::json!(false);
                report["bitwise_parity"] = serde_json::json!(false);
                report["verdict"] = serde_json::json!("infrastructure_error");
                report["infrastructure_error"] =
                    serde_json::json!({"kind": "skid_overshoot", "count": 0});
            }
            _ => unreachable!(),
        }
        write_report(&report_path, &report);
        let output = Command::new(env!("CARGO_BIN_EXE_verification-report"))
            .args(["--json", "matched"])
            .arg(&report_path)
            .output()
            .expect("refuse malformed report");
        assert_eq!(output.status.code(), Some(2), "{malformed}: {output:?}");
        assert!(output.stdout.is_empty(), "{malformed}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("REFUSED"),
            "{malformed}: {output:?}"
        );
    }
    fs::remove_dir_all(temporary).expect("remove temporary directory");
}
