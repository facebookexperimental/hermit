#!/usr/bin/env -S rust-script --force
//! Construct and read dagrun's shared structured test-result record.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! serde_json = "1"
//! ```

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use dagrun::TestResult;
use dagrun::TestResults;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

fn usage() {
    println!(
        "usage:\n  structured-test-results.rs write OUTPUT_OR_DASH EXECUTED FILTERED [ID pass|fail ATTEMPTS]...\n  structured-test-results.rs read INPUT\n  structured-test-results.rs summary INPUT"
    );
}

fn parse_u64(value: String, field: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|error| format!("structured-test-results-{field}: {error}"))
}

fn write(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let output = args
        .next()
        .ok_or_else(|| "structured-test-results-write: missing OUTPUT".to_string())?;
    let executed_tests = parse_u64(
        args.next()
            .ok_or_else(|| "structured-test-results-write: missing EXECUTED".to_string())?,
        "executed_tests",
    )?;
    let filtered_tests = parse_u64(
        args.next()
            .ok_or_else(|| "structured-test-results-write: missing FILTERED".to_string())?,
        "filtered_tests",
    )?;

    let remaining = args.collect::<Vec<_>>();
    let (rows, remainder) = remaining.as_chunks::<3>();
    if !remainder.is_empty() {
        return Err(
            "structured-test-results-write: terminal rows require ID, pass|fail, and ATTEMPTS"
                .into(),
        );
    }
    let mut results = Vec::with_capacity(rows.len());
    for fields in rows {
        let passed = match fields[1].as_str() {
            "pass" => true,
            "fail" => false,
            value => {
                return Err(format!(
                    "structured-test-results-result has unknown value {value:?}"
                ));
            }
        };
        let attempts = parse_u64(fields[2].clone(), "attempts")?;
        results.push(TestResult::new(fields[0].clone(), passed, attempts)?);
    }
    let report = TestResults::current(executed_tests, filtered_tests, results)?;
    if output == "-" {
        // Callers without a scheduler-owned output path historically validate
        // their arguments without writing or printing a record.
        report.to_current_json()?;
        Ok(())
    } else {
        report.write_current(Path::new(&output))
    }
}

fn read_report(mut args: impl Iterator<Item = String>) -> Result<TestResults, String> {
    let input = args
        .next()
        .ok_or_else(|| "structured-test-results-read: missing INPUT".to_string())?;
    if args.next().is_some() {
        return Err("structured-test-results-read: unexpected argument".into());
    }
    let bytes = fs::read(&input)
        .map_err(|error| format!("structured-test-results-read {input}: {error}"))?;
    let report = TestResults::from_json_slice(&bytes)?;
    report.to_current_json()?;
    Ok(report)
}

fn read(args: impl Iterator<Item = String>) -> Result<(), String> {
    let report = read_report(args)?;
    let current = report.to_current_json()?;
    println!(
        "{}",
        String::from_utf8(current).expect("TestResults JSON is UTF-8")
    );
    Ok(())
}

fn summary_value(report: &TestResults) -> serde_json::Value {
    let results = report
        .results
        .as_ref()
        .expect("read_report refuses retained count-only rows");
    let passed_tests = results.iter().filter(|result| result.passed).count();
    let failed_tests = results.len() - passed_tests;
    let first_failed_test = results
        .iter()
        .find(|result| !result.passed)
        .map(|result| result.id.as_str());
    serde_json::json!({
        "executed_tests": report.executed_tests,
        "filtered_tests": report.filtered_tests,
        "passed_tests": passed_tests,
        "failed_tests": failed_tests,
        "first_failed_test": first_failed_test,
    })
}

fn summary(args: impl Iterator<Item = String>) -> Result<(), String> {
    let report = read_report(args)?;
    println!("{}", summary_value(&report));
    Ok(())
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
        return Err("structured-test-results: missing command".into());
    };
    match command.as_str() {
        "write" => write(args),
        "read" => read(args),
        "summary" => summary(args),
        "--help" | "-h" => {
            usage();
            Ok(())
        }
        value => Err(format!(
            "structured-test-results: unknown command {value:?}"
        )),
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("test-results: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    fn repository_root() -> PathBuf {
        let source = PathBuf::from(file!());
        let source = if source.is_absolute() {
            source
        } else {
            env::current_dir().unwrap().join(source)
        };
        source.parent().unwrap().parent().unwrap().to_path_buf()
    }

    #[test]
    fn writer_constructs_the_shared_type() {
        let path = env::temp_dir().join(format!(
            "hermit-test-results-write-{}.json",
            std::process::id()
        ));
        write(
            [
                path.display().to_string(),
                "2".into(),
                "3".into(),
                "suite$pass".into(),
                "pass".into(),
                "1".into(),
                "suite$fail".into(),
                "fail".into(),
                "2".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        let bytes = fs::read(&path).unwrap();
        let report = TestResults::from_json_slice(&bytes).unwrap();
        assert_eq!(report.executed_tests, 2);
        assert_eq!(report.filtered_tests, 3);
        assert_eq!(
            report.results.unwrap(),
            vec![
                TestResult::new("suite$pass".into(), true, 1).unwrap(),
                TestResult::new("suite$fail".into(), false, 2).unwrap(),
            ]
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn writer_refuses_an_unknown_result_value() {
        let error = write(
            [
                "/tmp/unused-test-results.json".into(),
                "1".into(),
                "0".into(),
                "suite$case".into(),
                "unknown".into(),
                "1".into(),
            ]
            .into_iter(),
        )
        .unwrap_err();
        assert!(error.contains("unknown value \"unknown\""), "{error}");
    }

    #[test]
    fn dash_validates_without_printing_or_writing() {
        write(
            [
                "-".into(),
                "1".into(),
                "0".into(),
                "suite$case".into(),
                "pass".into(),
                "1".into(),
            ]
            .into_iter(),
        )
        .unwrap();
    }

    #[test]
    fn summary_is_derived_from_the_shared_terminal_rows() {
        let report = TestResults::current(
            3,
            4,
            vec![
                TestResult::new("suite$one".into(), true, 1).unwrap(),
                TestResult::new("suite$two".into(), false, 2).unwrap(),
                TestResult::new("suite$three".into(), false, 1).unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(
            summary_value(&report),
            serde_json::json!({
                "executed_tests": 3,
                "filtered_tests": 4,
                "passed_tests": 1,
                "failed_tests": 2,
                "first_failed_test": "suite$two",
            })
        );
    }

    #[test]
    fn retained_count_only_rows_have_no_current_read_output() {
        let retained =
            TestResults::from_json_slice(br#"{"schema":1,"executed_tests":7,"filtered_tests":11}"#)
                .unwrap();
        assert!(
            retained
                .to_current_json()
                .unwrap_err()
                .contains("retained schema 1 has no current write path")
        );
    }

    #[test]
    fn shell_producers_and_consumers_keep_the_typed_channel_controls() {
        let root = repository_root();
        for script in [
            "ci/write-structured-test-counts.sh",
            "scripts/test-fail-closed.sh",
            "scripts/progress-report.sh",
            "scripts/compat-map.sh",
        ] {
            let output = Command::new("bash")
                .arg(root.join(script))
                .arg("--self-test")
                .current_dir(&root)
                .output()
                .unwrap_or_else(|error| panic!("cannot run {script} --self-test: {error}"));
            assert!(
                output.status.success(),
                "{script} --self-test failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
}
