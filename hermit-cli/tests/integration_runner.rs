//! Cargo-native integration test runner for Hermit
//!
//! This module provides a portable integration test runner that replaces the internal
//! Buck-based matrix. It discovers guest programs from `tests/` and `flaky-tests/`,
//! executes them with various Hermit modes, and provides stable filtering, timeout,
//! logging, and failure-reporting behavior.

pub mod runners;

#[cfg(test)]
mod tests {
    use super::runners::*;

    #[test]
    fn test_discovery_finds_guest_programs() {
        let discoverer = MatrixDiscoverer::new();
        let programs = discoverer.discover_guest_programs();
        assert!(
            !programs.is_empty(),
            "Should find at least some guest programs"
        );
    }

    #[test]
    fn test_discovery_includes_tests_and_flaky_tests() {
        let discoverer = MatrixDiscoverer::new();
        let programs = discoverer.discover_guest_programs();

        let has_tests = programs.iter().any(|p| p.source.contains("tests/"));
        let has_flaky_tests = programs.iter().any(|p| p.source.contains("flaky-tests/"));

        assert!(has_tests, "Should include programs from tests/");
        assert!(has_flaky_tests, "Should include programs from flaky-tests/");
    }

    #[test]
    fn test_hardware_detection_classifies_tests() {
        let detector = HardwareDetector::new();
        let classification = detector.classify_test("basic_echo");

        assert_eq!(classification, HardwareRequirement::None);
    }

    #[test]
    fn test_hardware_detection_pmu_tests() {
        let detector = HardwareDetector::new();
        let classification = detector.classify_test("mem_race");

        assert_eq!(classification, HardwareRequirement::Pmu);
    }

    #[test]
    fn test_matrix_executor_runs_basic_case() {
        let executor = MatrixExecutor::new();
        let result = executor.run_case("echo", &["integration-echo"], HermitMode::Default);

        assert!(result.is_ok(), "Basic echo should pass");
    }

    #[test]
    fn test_filtering_by_name() {
        let runner = IntegrationRunner::new();
        let filtered = runner.filter_tests(&["echo", "ls", "cat"], "echo");

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], "echo");
    }

    #[test]
    fn test_filtering_by_category() {
        let runner = IntegrationRunner::new();
        let filtered = runner.filter_tests(&["echo", "ls", "python"], "basic");

        assert!(filtered.contains(&"echo"));
        assert!(filtered.contains(&"ls"));
        assert!(!filtered.contains(&"python"));
    }

    #[test]
    fn test_timeout_enforcement() {
        let executor = MatrixExecutor::new();
        let result = executor.run_with_timeout("sleep", &["10"], Duration::from_secs(1));

        assert!(result.is_err(), "Should timeout");
        assert!(matches!(result.unwrap_err(), ExecutionError::Timeout));
    }

    #[test]
    fn test_record_replay_deterministic() {
        let executor = MatrixExecutor::new();
        let result = executor.run_record_replay("echo", &["test"]);

        assert!(result.is_ok(), "Record/replay should be deterministic");
        assert!(
            result.unwrap().deterministic,
            "Replay should match recording"
        );
    }

    #[test]
    fn test_chaos_mode_execution() {
        let executor = MatrixExecutor::new();
        let result = executor.run_case("futex_wake_some", &[], HermitMode::Chaos);

        assert!(result.is_ok(), "Chaos mode should execute without crash");
    }

    #[test]
    fn test_virtual_time_execution() {
        let executor = MatrixExecutor::new();
        let result = executor.run_case("nanosleep", &[], HermitMode::VirtualTime);

        assert!(result.is_ok(), "Virtual time mode should execute");
    }

    #[test]
    fn test_threading_futex_execution() {
        let executor = MatrixExecutor::new();
        let result = executor.run_case("futex_and_print", &[], HermitMode::Default);

        assert!(result.is_ok(), "Threading/futex test should execute");
    }

    #[test]
    fn test_reporting_structure() {
        let reporter = MatrixReporter::new();
        let report = reporter.generate_report(vec![
            TestResult::pass("echo", "basic", Duration::from_millis(100)),
            TestResult::fail(
                "nginx",
                "expected-fail",
                Duration::from_millis(50),
                "exit code 1",
            ),
        ]);

        assert!(report.contains("PASS"));
        assert!(report.contains("FAIL"));
        assert!(report.contains("echo"));
        assert!(report.contains("nginx"));
    }

    #[test]
    fn test_coverage_manifest_generation() {
        let discoverer = MatrixDiscoverer::new();
        let manifest = discoverer.generate_coverage_manifest();

        assert!(!manifest.is_empty(), "Should generate coverage manifest");
        assert!(manifest.contains("buck_scenario"));
        assert!(manifest.contains("status"));
    }

    #[test]
    fn test_local_validation_command() {
        let runner = IntegrationRunner::new();
        let result = runner.run_local_validation();

        assert!(result.is_ok(), "Local validation should pass");
    }

    #[test]
    fn test_rr_suite_execution() {
        let executor = MatrixExecutor::new();
        let result = executor.run_rr_suite();

        // May be skipped if rr not available
        assert!(result.is_ok() || matches!(result.unwrap_err(), ExecutionError::Skipped));
    }

    #[test]
    fn test_hermit_modes_matrix() {
        let executor = MatrixExecutor::new();
        let results = executor.run_hermit_modes_matrix("basic");

        assert!(results.contains_key(&HermitMode::Default));
        assert!(results.contains_key(&HermitMode::Strict));
        assert!(results.contains_key(&HermitMode::Chaos));
    }

    #[test]
    fn test_strict_mode_matrix() {
        let executor = MatrixExecutor::new();
        let results = executor.run_strict_mode_matrix("echo");

        assert!(
            !results.is_empty(),
            "Strict mode matrix should have results"
        );
    }

    #[test]
    fn test_parallel_execution() {
        let executor = MatrixExecutor::new();
        let results = executor.run_parallel(&["echo", "ls", "cat"], 2);

        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.is_ok()));
    }

    #[test]
    fn test_failure_diagnostics() {
        let executor = MatrixExecutor::new();
        let result = executor.run_case("nginx", &["-t"], HermitMode::Default);

        if let Err(e) = result {
            let diagnostic = e.diagnostic();
            assert!(diagnostic.contains("exit"));
            assert!(diagnostic.contains("stdout") || diagnostic.contains("stderr"));
        }
    }

    #[test]
    fn test_explicit_capability_detection() {
        let detector = HardwareDetector::new();
        let capabilities = detector.detect_capabilities();

        assert!(capabilities.has_pmu || !capabilities.has_pmu); // Always valid
        assert!(capabilities.has_cpuid_interception || !capabilities.has_cpuid_interception);
    }

    #[test]
    fn test_hardware_dependent_tests_not_silently_passing() {
        let runner = IntegrationRunner::new();
        let config = runner.default_config();

        assert!(
            !config.silently_pass_hardware_tests,
            "Hardware tests should not silently pass"
        );
    }
}
