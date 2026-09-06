use std::process::Command;
use std::process::Output;

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_verification-report"))
        .args(arguments)
        .output()
        .expect("failed to execute verification-report")
}

#[test]
fn conventional_help_is_successful_and_consistent() {
    let short = run(&["-h"]);
    let long = run(&["--help"]);
    for output in [&short, &long] {
        assert!(
            output.status.success(),
            "help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with("Usage: verification-report"), "{stdout}");
        assert!(stdout.contains("matched"), "{stdout}");
        assert!(stdout.contains("canonical-match"), "{stdout}");
        assert!(stdout.contains("--json"), "{stdout}");
        assert!(
            stdout.contains("does not mean verification passed"),
            "{stdout}"
        );
        assert!(output.stderr.is_empty(), "help wrote stderr: {output:?}");
    }
    assert_eq!(short.stdout, long.stdout, "-h and --help must agree");
}

#[test]
fn missing_arguments_remain_a_refusal() {
    let output = run(&[]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("REFUSED"), "{stderr}");
    assert!(stderr.contains("usage:"), "{stderr}");
    assert!(output.stdout.is_empty(), "{output:?}");
}
#[test]
fn no_result_prints_the_typed_cause() {
    let report = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        report.path(),
        serde_json::to_vec(&serde_json::json!({
            "verified": false,
            "bitwise_parity": false,
            "verdict": "no_result",
            "no_result_reason": {
                "kind": "comparison_refused",
                "detail": "the first log was truncated at the configured size bound"
            },
            "infrastructure_error": null,
            "comparison": null,
            "compared_log_messages": null,
            "guest_exit_code": 1,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        }))
        .unwrap(),
    )
    .unwrap();
    let output = run(&["matched", report.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the first log was truncated at the configured size bound"),
        "{stderr}"
    );
}

#[test]
fn no_result_prints_explicit_absence_without_calling_it_omission() {
    let report = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        report.path(),
        serde_json::to_vec(&serde_json::json!({
            "verified": false,
            "bitwise_parity": false,
            "verdict": "no_result",
            "no_result_reason": null,
            "infrastructure_error": null,
            "comparison": null,
            "compared_log_messages": null,
            "guest_exit_code": 1,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        }))
        .unwrap(),
    )
    .unwrap();
    let output = run(&["matched", report.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("the producer recorded no specific no-result cause"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("omitted"),
        "explicit null must not be reported as a missing field: {stderr}"
    );
}
