//! Main Cargo integration runner - ties all components together and provides CLI entry point

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use super::hardware_detection::HardwareDetector;
use super::hardware_detection::HardwareRequirement;
use super::matrix_discovery::GuestProgram;
use super::matrix_discovery::HermitMode;
use super::matrix_discovery::MatrixDiscoverer;
use super::matrix_discovery::TestScenario;
use super::matrix_executor::ExecutionResult;
use super::matrix_executor::MatrixExecutor;
use super::matrix_reporting::MatrixReporter;
use super::matrix_reporting::TestResult;
use super::matrix_reporting::TestStatus;

/// Configuration for the integration runner
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub filter: Option<String>,
    pub category_filter: Option<String>,
    pub mode_filter: Option<Vec<HermitMode>>,
    pub max_parallel: usize,
    pub timeout_override: Option<Duration>,
    pub silently_pass_hardware_tests: bool,
    pub output_format: OutputFormat,
    pub output_file: Option<PathBuf>,
    pub generate_manifest: bool,
    pub dry_run: bool,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            filter: None,
            category_filter: None,
            mode_filter: None,
            max_parallel: 4,
            timeout_override: None,
            silently_pass_hardware_tests: false,
            output_format: OutputFormat::Human,
            output_file: None,
            generate_manifest: false,
            dry_run: false,
        }
    }
}

/// Output format for reports
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
    Junit,
}

/// Main integration runner
pub struct IntegrationRunner {
    config: RunnerConfig,
    discoverer: MatrixDiscoverer,
    executor: MatrixExecutor,
    reporter: MatrixReporter,
    detector: HardwareDetector,
}

impl IntegrationRunner {
    pub fn new(config: RunnerConfig) -> Self {
        Self {
            config,
            discoverer: MatrixDiscoverer::new(),
            executor: MatrixExecutor::new(),
            reporter: MatrixReporter::new(),
            detector: HardwareDetector::new(),
        }
    }

    pub fn default_config() -> RunnerConfig {
        RunnerConfig::default()
    }

    /// Run the full integration test suite
    pub fn run(&mut self) -> Result<Vec<TestResult>, String> {
        // Generate test matrix
        let scenarios = self.discoverer.generate_test_matrix();

        // Apply filters
        let scenarios = self.apply_filters(scenarios);

        if self.config.dry_run {
            println!("DRY RUN: Would execute {} scenarios", scenarios.len());
            for scenario in &scenarios {
                println!(
                    "  {} ({:?}) [{:?}]",
                    scenario.guest.name, scenario.hermit_mode, scenario.hardware_requirement
                );
            }
            return Ok(Vec::new());
        }

        // Generate manifest if requested
        if self.config.generate_manifest {
            let manifest = self.discoverer.generate_coverage_manifest();
            if let Some(file) = &self.config.output_file {
                std::fs::write(file, manifest).map_err(|e| e.to_string())?;
            } else {
                println!("{}", manifest);
            }
            return Ok(Vec::new());
        }

        // Run tests
        let results = if self.config.max_parallel > 1 {
            self.run_parallel(scenarios)
        } else {
            self.run_sequential(scenarios)
        }?;

        // Generate report
        self.generate_output(&results)?;

        Ok(results)
    }

    /// Apply filters to test scenarios
    fn apply_filters(&self, scenarios: Vec<TestScenario>) -> Vec<TestScenario> {
        scenarios
            .into_iter()
            .filter(|scenario| {
                // Name filter
                if let Some(filter) = &self.config.filter {
                    if !scenario.guest.name.contains(filter) {
                        return false;
                    }
                }

                // Category filter
                if let Some(cat_filter) = &self.config.category_filter {
                    if scenario.guest.category != *cat_filter {
                        return false;
                    }
                }

                // Mode filter
                if let Some(mode_filter) = &self.config.mode_filter {
                    if !mode_filter.contains(&scenario.hermit_mode) {
                        return false;
                    }
                }

                // Hardware filter - skip hardware tests if not explicitly allowed
                if !self.config.silently_pass_hardware_tests {
                    if scenario.hardware_requirement != HardwareRequirement::None {
                        if !self.detector.can_run_test(&scenario.guest.name) {
                            return false;
                        }
                    }
                }

                true
            })
            .collect()
    }

    /// Run scenarios sequentially
    fn run_sequential(&mut self, scenarios: Vec<TestScenario>) -> Result<Vec<TestResult>, String> {
        let mut results = Vec::new();

        for scenario in scenarios {
            let result = self.run_single_scenario(&scenario);
            results.push(result);
        }

        Ok(results)
    }
    /// Run scenarios in parallel
    fn run_parallel(&mut self, scenarios: Vec<TestScenario>) -> Result<Vec<TestResult>, String> {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::thread;

        let scenarios = Arc::new(scenarios);
        let results = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(self.executor.clone());
        let detector = Arc::new(self.detector.clone());
        let config = self.config.clone();

        // Simple semaphore using a channel for limiting concurrency
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..config.max_parallel {
            tx.send(()).unwrap();
        }
        let semaphore = Arc::new((tx, rx));

        let mut handles = Vec::new();

        for scenario in scenarios.iter().cloned() {
            let executor = executor.clone();
            let detector = detector.clone();
            let results = results.clone();
            let semaphore = semaphore.clone();

            let handle = thread::spawn(move || {
                // Acquire semaphore permit
                let (_tx, ref rx) = *semaphore;
                let _permit = rx.recv().unwrap();

                // Check hardware requirements
                if !config.silently_pass_hardware_tests {
                    if scenario.hardware_requirement != HardwareRequirement::None {
                        if !detector.can_run_test(&scenario.guest.name) {
                            let reason = detector
                                .skip_reason(&scenario.guest.name)
                                .unwrap_or_else(|| "Hardware requirement not met".to_string());
                            results.lock().unwrap().push(TestResult::hardware_skipped(
                                &scenario.guest.name,
                                &scenario.guest.category,
                                scenario.hermit_mode,
                                scenario.hardware_requirement,
                                &reason,
                            ));
                            return;
                        }
                    }
                }

                let result = executor.run_scenario(&scenario);
                let test_result = match result {
                    Ok(exec_result) => {
                        let status = if exec_result.exit_code == Some(0) {
                            TestStatus::Pass
                        } else {
                            TestStatus::Fail
                        };
                        TestResult {
                            name: scenario.guest.name.clone(),
                            category: scenario.guest.category.clone(),
                            mode: scenario.hermit_mode,
                            status,
                            duration: exec_result.duration,
                            detail: format!(
                                "exit={:?}, output={}",
                                exec_result.exit_code,
                                if exec_result.stdout == exec_result.stderr {
                                    "match"
                                } else {
                                    "DIFF"
                                }
                            ),
                            diagnostic: if exec_result.exit_code != Some(0) {
                                Some(format!(
                                    "stdout:\n{}\nstderr:\n{}",
                                    exec_result.stdout, exec_result.stderr
                                ))
                            } else {
                                None
                            },
                            hardware_requirement: scenario.hardware_requirement,
                        }
                    }
                    Err(e) => TestResult::fail(
                        &scenario.guest.name,
                        &scenario.guest.category,
                        scenario.hermit_mode,
                        Duration::from_millis(0),
                        &e.to_string(),
                    ),
                };

                results.lock().unwrap().push(test_result);
            });

            handles.push(handle);
        }

        for handle in handles {
            handle.join().ok();
        }

        let mut results_vec = results.lock().unwrap().drain(..).collect::<Vec<_>>();
        results_vec.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| format!("{:?}", a.mode).cmp(&format!("{:?}", b.mode)))
        });
        Ok(results_vec)
    }

    /// Run a single scenario and convert to TestResult
    fn run_single_scenario(&self, scenario: &TestScenario) -> TestResult {
        // Check hardware requirements
        if !self.config.silently_pass_hardware_tests {
            if scenario.hardware_requirement != HardwareRequirement::None {
                if !self.detector.can_run_test(&scenario.guest.name) {
                    let reason = self
                        .detector
                        .skip_reason(&scenario.guest.name)
                        .unwrap_or_else(|| "Hardware requirement not met".to_string());
                    return TestResult::hardware_skipped(
                        &scenario.guest.name,
                        &scenario.guest.category,
                        scenario.hermit_mode,
                        scenario.hardware_requirement,
                        &reason,
                    );
                }
            }
        }

        let result = self.executor.run_scenario(scenario);
        match result {
            Ok(exec_result) => {
                let status = if exec_result.exit_code == Some(0) {
                    TestStatus::Pass
                } else {
                    TestStatus::Fail
                };
                TestResult {
                    name: scenario.guest.name.clone(),
                    category: scenario.guest.category.clone(),
                    mode: scenario.hermit_mode,
                    status,
                    duration: exec_result.duration,
                    detail: format!(
                        "exit={:?}, output={}",
                        exec_result.exit_code,
                        if exec_result.stdout == exec_result.stderr {
                            "match"
                        } else {
                            "DIFF"
                        }
                    ),
                    diagnostic: if exec_result.exit_code != Some(0) {
                        Some(format!(
                            "stdout:\n{}\nstderr:\n{}",
                            exec_result.stdout, exec_result.stderr
                        ))
                    } else {
                        None
                    },
                    hardware_requirement: scenario.hardware_requirement,
                }
            }
            Err(e) => TestResult::fail(
                &scenario.guest.name,
                &scenario.guest.category,
                scenario.hermit_mode,
                Duration::from_millis(0),
                &e.to_string(),
            ),
        }
    }

    /// Generate output based on configuration
    fn generate_output(&self, results: &[TestResult]) -> Result<(), String> {
        let output = match self.config.output_format {
            OutputFormat::Human => self.reporter.generate_report(results),
            OutputFormat::Json => self.reporter.generate_json_report(results),
            OutputFormat::Junit => self.reporter.generate_junit_xml(results),
        };

        if let Some(file) = &self.config.output_file {
            std::fs::write(file, output).map_err(|e| e.to_string())?;
        } else {
            println!("{}", output);
        }

        Ok(())
    }

    /// Run local validation (quick sanity check)
    pub fn run_local_validation(&self) -> Result<(), String> {
        let programs = self.discoverer.discover_guest_programs();
        let basic_programs: Vec<_> = programs
            .iter()
            .filter(|p| p.category == "basic")
            .take(3)
            .collect();

        for program in basic_programs {
            let scenario = TestScenario {
                guest: program.clone(),
                hermit_mode: HermitMode::Default,
                timeout_seconds: 30,
                hardware_requirement: self.detector.classify_test(&program.name),
            };

            let result = self.executor.run_scenario(&scenario);
            if result.is_err() {
                return Err(format!(
                    "Local validation failed for {}: {}",
                    program.name,
                    result.unwrap_err()
                ));
            }
        }

        println!(
            "Local validation passed for {} basic programs",
            basic_programs.len()
        );
        Ok(())
    }

    /// Filter tests by name pattern
    pub fn filter_tests(&self, names: &[&str], pattern: &str) -> Vec<String> {
        names
            .iter()
            .filter(|n| n.contains(pattern))
            .map(|s| s.to_string())
            .collect()
    }
}

impl Clone for IntegrationRunner {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            discoverer: MatrixDiscoverer::new(),
            executor: MatrixExecutor::new(),
            reporter: MatrixReporter::new(),
            detector: HardwareDetector::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runner_creation() {
        let config = RunnerConfig::default();
        let runner = IntegrationRunner::new(config);
        // Should not panic
    }

    #[test]
    fn test_filter_tests_by_name() {
        let runner = IntegrationRunner::new(RunnerConfig::default());
        let filtered = runner.filter_tests(&["echo", "ls", "cat"], "echo");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], "echo");
    }

    #[test]
    fn test_filter_tests_by_category() {
        let runner = IntegrationRunner::new(RunnerConfig::default());
        let filtered = runner.filter_tests(&["echo", "ls", "python"], "basic");
        // This tests the filter_tests helper, not the actual category filtering
        assert!(filtered.contains(&"echo"));
        assert!(filtered.contains(&"ls"));
        assert!(!filtered.contains(&"python"));
    }

    #[test]
    fn test_default_config() {
        let config = IntegrationRunner::default_config();
        assert!(!config.silently_pass_hardware_tests);
        assert_eq!(config.max_parallel, 4);
        assert_eq!(config.output_format, OutputFormat::Human);
    }
}
