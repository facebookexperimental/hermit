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
