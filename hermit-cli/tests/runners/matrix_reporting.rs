//! Matrix reporting - structured output with failure diagnostics

use std::collections::HashMap;
use std::time::Duration;

use super::hardware_detection::HardwareRequirement;
use super::matrix_discovery::GuestProgram;
use super::matrix_discovery::HermitMode;
use super::matrix_discovery::TestScenario;
use super::matrix_executor::ExecutionError;
use super::matrix_executor::ExecutionResult;

/// Test result for reporting
#[derive(Debug, Clone)]
pub struct TestResult {
    pub name: String,
    pub category: String,
    pub mode: HermitMode,
    pub status: TestStatus,
    pub duration: Duration,
    pub detail: String,
    pub diagnostic: Option<String>,
    pub hardware_requirement: HardwareRequirement,
}

/// Test status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestStatus {
    Pass,
    Fail,
    ExpectedFail,
    UnexpectedPass,
    Skip,
    HardwareSkipped,
}

impl TestStatus {
    pub fn is_failure(&self) -> bool {
        matches!(self, TestStatus::Fail | TestStatus::UnexpectedPass)
    }
}

impl std::fmt::Display for TestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TestStatus::Pass => "PASS",
            TestStatus::Fail => "FAIL",
            TestStatus::ExpectedFail => "XFAIL",
            TestStatus::UnexpectedPass => "XPASS",
            TestStatus::Skip => "SKIP",
            TestStatus::HardwareSkipped => "HWSKIP",
        })
    }
}

impl TestResult {
    pub fn pass(name: &str, category: &str, mode: HermitMode, duration: Duration) -> Self {
        Self {
            name: name.to_string(),
            category: category.to_string(),
            mode,
            status: TestStatus::Pass,
            duration,
            detail: format!("exit=Some(0), output=match"),
            diagnostic: None,
            hardware_requirement: HardwareRequirement::None,
        }
    }

    pub fn fail(
        name: &str,
        category: &str,
        mode: HermitMode,
        duration: Duration,
        detail: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            category: category.to_string(),
            mode,
            status: TestStatus::Fail,
            duration,
            detail: detail.to_string(),
            diagnostic: Some(detail.to_string()),
            hardware_requirement: HardwareRequirement::None,
        }
    }

    pub fn skip(name: &str, category: &str, mode: HermitMode, reason: &str) -> Self {
        Self {
            name: name.to_string(),
            category: category.to_string(),
            mode,
            status: TestStatus::Skip,
            duration: Duration::from_millis(0),
            detail: reason.to_string(),
            diagnostic: None,
            hardware_requirement: HardwareRequirement::None,
        }
    }

    pub fn hardware_skipped(
        name: &str,
        category: &str,
        mode: HermitMode,
        requirement: HardwareRequirement,
        reason: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            category: category.to_string(),
            mode,
            status: TestStatus::HardwareSkipped,
            duration: Duration::from_millis(0),
            detail: reason.to_string(),
            diagnostic: Some(reason.to_string()),
            hardware_requirement: requirement,
        }
    }
}

/// Matrix reporter for generating structured output
pub struct MatrixReporter {
    results: Vec<TestResult>,
    start_time: std::time::Instant,
}

impl MatrixReporter {
    pub fn new() -> Self {
        Self {
            results: Vec::new(),
            start_time: std::time::Instant::now(),
        }
    }

    pub fn add_result(&mut self, result: TestResult) {
        self.results.push(result);
    }

    pub fn add_results(&mut self, results: Vec<TestResult>) {
        self.results.extend(results);
    }

    /// Generate human-readable matrix report
    pub fn generate_report(&self, results: &[TestResult]) -> String {
        let mut report = String::new();

        report.push_str("\n");
        report.push_str("Hermit Integration Test Matrix Report\n");
        report.push_str("=====================================\n\n");

        // Summary
        let total = results.len();
        let passed = results
            .iter()
            .filter(|r| r.status == TestStatus::Pass)
            .count();
        let failed = results.iter().filter(|r| r.status.is_failure()).count();
        let skipped = results
            .iter()
            .filter(|r| r.status == TestStatus::Skip)
            .count();
        let hw_skipped = results
            .iter()
            .filter(|r| r.status == TestStatus::HardwareSkipped)
            .count();
        let xfail = results
            .iter()
            .filter(|r| r.status == TestStatus::ExpectedFail)
            .count();
        let xpass = results
            .iter()
            .filter(|r| r.status == TestStatus::UnexpectedPass)
            .count();

        report.push_str(&format!("Summary: {} total, {} passed, {} failed, {} skipped, {} hw_skipped, {} xfail, {} xpass\n\n",
            total, passed, failed, skipped, hw_skipped, xfail, xpass));

        // Table header
        report.push_str(&format!(
            "{:<14} {:<20} {:<12} {:<8} {:>12}  detail\n",
            "category", "program", "mode", "result", "time"
        ));
        report.push_str(&"-".repeat(90));
        report.push_str("\n");

        // Results grouped by category
        let mut by_category: HashMap<String, Vec<&TestResult>> = HashMap::new();
        for result in results {
            by_category
                .entry(result.category.clone())
                .or_default()
                .push(result);
        }

        let mut categories: Vec<_> = by_category.keys().cloned().collect();
        categories.sort();

        for category in categories {
            let results = by_category.get(&category).unwrap();
            for result in results {
                report.push_str(&format!(
                    "{:<14} {:<20} {:<12} {:<8} {:>10}ms  {}\n",
                    category,
                    &result.name[..result.name.len().min(20)],
                    mode_short_name(result.mode),
                    result.status,
                    result.duration.as_millis(),
                    result.detail
                ));

                if let Some(diag) = &result.diagnostic {
                    report.push_str(&format!("{:<56}  DIAG: {}\n", "", diag));
                }
            }
        }

        report.push_str("\n");

        // Failures detail
        let failures: Vec<_> = results.iter().filter(|r| r.status.is_failure()).collect();
        if !failures.is_empty() {
            report.push_str("FAILURES:\n");
            report.push_str("---------\n");
            for failure in failures {
                report.push_str(&format!("\n{} ({:?}):\n", failure.name, failure.mode));
                if let Some(diag) = &failure.diagnostic {
                    report.push_str(diag);
                    report.push_str("\n");
                }
            }
        }

        // Hardware skipped detail
        let hw_skipped: Vec<_> = results
            .iter()
            .filter(|r| r.status == TestStatus::HardwareSkipped)
            .collect();
        if !hw_skipped.is_empty() {
            report.push_str("\nHARDWARE SKIPPED:\n");
            report.push_str("-----------------\n");
            for skipped in hw_skipped {
                report.push_str(&format!(
                    "  {} ({:?}): {} [{:?}]\n",
                    skipped.name, skipped.mode, skipped.detail, skipped.hardware_requirement
                ));
            }
        }

        report.push_str(&format!(
            "\nTotal time: {:.2}s\n",
            self.start_time.elapsed().as_secs_f64()
        ));

        report
    }

    /// Generate JSON report for CI integration
    pub fn generate_json_report(&self, results: &[TestResult]) -> String {
        use serde_json::json;

        let mut by_category: HashMap<String, Vec<&TestResult>> = HashMap::new();
        for result in results {
            by_category
                .entry(result.category.clone())
                .or_default()
                .push(result);
        }

        let mut category_objects = Vec::new();
        for (category, results) in by_category {
            let mut test_objects = Vec::new();
            for result in results {
                test_objects.push(json!({
                    "name": result.name,
                    "mode": format!("{:?}", result.mode),
                    "status": format!("{:?}", result.status),
                    "duration_ms": result.duration.as_millis(),
                    "detail": result.detail,
                    "diagnostic": result.diagnostic,
                    "hardware_requirement": format!("{:?}", result.hardware_requirement),
                }));
            }
            category_objects.push(json!({
                "category": category,
                "tests": test_objects,
            }));
        }

        let summary = json!({
            "total": results.len(),
            "passed": results.iter().filter(|r| r.status == TestStatus::Pass).count(),
            "failed": results.iter().filter(|r| r.status.is_failure()).count(),
            "skipped": results.iter().filter(|r| r.status == TestStatus::Skip).count(),
            "hardware_skipped": results.iter().filter(|r| r.status == TestStatus::HardwareSkipped).count(),
            "expected_fail": results.iter().filter(|r| r.status == TestStatus::ExpectedFail).count(),
            "unexpected_pass": results.iter().filter(|r| r.status == TestStatus::UnexpectedPass).count(),
            "total_time_seconds": self.start_time.elapsed().as_secs_f64(),
            "categories": category_objects,
        });

        serde_json::to_string_pretty(&summary).unwrap_or_else(|_| "{}".to_string())
    }

    /// Generate JUnit XML for CI integration
    pub fn generate_junit_xml(&self, results: &[TestResult]) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push_str("\n");
        xml.push_str(&format!(
            r#"<testsuite name="hermit-integration-matrix" tests="{}" failures="{}" time="{:.3}">"#,
            results.len(),
            results.iter().filter(|r| r.status.is_failure()).count(),
            self.start_time.elapsed().as_secs_f64()
        ));
        xml.push_str("\n");

        for result in results {
            let classname = format!("hermit.integration.{}", result.category);
            let testname = format!("{}::{:?}", result.name, result.mode);
            let time = result.duration.as_secs_f64();

            xml.push_str(&format!(
                r#"  <testcase classname="{}" name="{}" time="{:.3}">"#,
                classname, testname, time
            ));
            xml.push_str("\n");

            match result.status {
                TestStatus::Pass => {
                    xml.push_str("  </testcase>\n");
                }
                TestStatus::Fail | TestStatus::UnexpectedPass => {
                    xml.push_str(&format!(
                        r#"    <failure message="{}">{}</failure>"#,
                        result.detail,
                        result.diagnostic.as_deref().unwrap_or("")
                    ));
                    xml.push_str("\n  </testcase>\n");
                }
                TestStatus::Skip | TestStatus::HardwareSkipped => {
                    xml.push_str(&format!(r#"    <skipped message="{}"/>"#, result.detail));
                    xml.push_str("\n  </testcase>\n");
                }
                TestStatus::ExpectedFail => {
                    // Expected failures are still passes in JUnit
                    xml.push_str("  </testcase>\n");
                }
            }
        }

        xml.push_str("</testsuite>\n");
        xml
    }
}

fn mode_short_name(mode: HermitMode) -> &'static str {
    match mode {
        HermitMode::Default => "default",
        HermitMode::Strict => "strict",
        HermitMode::Chaos => "chaos",
        HermitMode::VirtualTime => "vtime",
        HermitMode::VirtualRandom => "vrand",
        HermitMode::Record => "record",
        HermitMode::Replay => "replay",
        HermitMode::Verify => "verify",
    }
}

impl Default for MatrixReporter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reporter_creation() {
        let reporter = MatrixReporter::new();
        assert!(reporter.results.is_empty());
    }

    #[test]
    fn test_generate_report() {
        let reporter = MatrixReporter::new();
        let results = vec![
            TestResult::pass(
                "echo",
                "basic",
                HermitMode::Default,
                Duration::from_millis(100),
            ),
            TestResult::fail(
                "nginx",
                "expected-fail",
                HermitMode::Default,
                Duration::from_millis(50),
                "exit code 1",
            ),
        ];

        let report = reporter.generate_report(&results);

        assert!(report.contains("PASS"));
        assert!(report.contains("FAIL"));
        assert!(report.contains("echo"));
        assert!(report.contains("nginx"));
    }

    #[test]
    fn test_generate_json_report() {
        let reporter = MatrixReporter::new();
        let results = vec![TestResult::pass(
            "echo",
            "basic",
            HermitMode::Default,
            Duration::from_millis(100),
        )];

        let json = reporter.generate_json_report(&results);

        assert!(json.contains("echo"));
        assert!(json.contains("PASS"));
        assert!(json.contains("total"));
    }

    #[test]
    fn test_generate_junit_xml() {
        let reporter = MatrixReporter::new();
        let results = vec![
            TestResult::pass(
                "echo",
                "basic",
                HermitMode::Default,
                Duration::from_millis(100),
            ),
            TestResult::fail(
                "nginx",
                "expected-fail",
                HermitMode::Default,
                Duration::from_millis(50),
                "exit code 1",
            ),
        ];

        let xml = reporter.generate_junit_xml(&results);

        assert!(xml.contains("testsuite"));
        assert!(xml.contains("echo"));
        assert!(xml.contains("nginx"));
        assert!(xml.contains("failure"));
    }

    #[test]
    fn test_mode_short_names() {
        assert_eq!(mode_short_name(HermitMode::Default), "default");
        assert_eq!(mode_short_name(HermitMode::Strict), "strict");
        assert_eq!(mode_short_name(HermitMode::Chaos), "chaos");
        assert_eq!(mode_short_name(HermitMode::VirtualTime), "vtime");
        assert_eq!(mode_short_name(HermitMode::VirtualRandom), "vrand");
        assert_eq!(mode_short_name(HermitMode::Record), "record");
        assert_eq!(mode_short_name(HermitMode::Replay), "replay");
        assert_eq!(mode_short_name(HermitMode::Verify), "verify");
    }

    #[test]
    fn test_test_status_display() {
        assert_eq!(format!("{}", TestStatus::Pass), "PASS");
        assert_eq!(format!("{}", TestStatus::Fail), "FAIL");
        assert_eq!(format!("{}", TestStatus::ExpectedFail), "XFAIL");
        assert_eq!(format!("{}", TestStatus::UnexpectedPass), "XPASS");
        assert_eq!(format!("{}", TestStatus::Skip), "SKIP");
        assert_eq!(format!("{}", TestStatus::HardwareSkipped), "HWSKIP");
    }
}
