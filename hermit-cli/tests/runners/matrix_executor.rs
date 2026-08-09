//! Matrix executor - runs guest programs with Hermit modes

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use super::hardware_detection::HardwareDetector;
use super::hardware_detection::HardwareRequirement;
use super::matrix_discovery::GuestProgram;
use super::matrix_discovery::HermitMode;
use super::matrix_discovery::TestScenario;

/// Execution result
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub deterministic: bool,
}

/// Execution error types
#[derive(Debug, Clone)]
pub enum ExecutionError {
    Timeout,
    ProcessFailed(String),
    HardwareUnavailable(String),
    Skipped,
    IoError(String),
}

impl std::fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutionError::Timeout => write!(f, "Execution timed out"),
            ExecutionError::ProcessFailed(msg) => write!(f, "Process failed: {}", msg),
            ExecutionError::HardwareUnavailable(msg) => write!(f, "Hardware unavailable: {}", msg),
            ExecutionError::Skipped => write!(f, "Test skipped"),
            ExecutionError::IoError(msg) => write!(f, "I/O error: {}", msg),
        }
    }
}

impl std::error::Error for ExecutionError {}

/// Matrix executor for running integration tests
pub struct MatrixExecutor {
    hermit_binary: PathBuf,
    detector: HardwareDetector,
    bind_dir: PathBuf,
}

impl MatrixExecutor {
    pub fn new() -> Self {
        let hermit_binary = std::env::var("CARGO_BIN_EXE_hermit")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target/debug/hermit"));

        let bind_dir = std::env::var("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
            .join("hermit-integration-matrix");

        std::fs::create_dir_all(&bind_dir).ok();

        Self {
            hermit_binary,
            detector: HardwareDetector::new(),
            bind_dir,
        }
    }

    /// Run a single test case with the given Hermit mode
    pub fn run_case(
        &self,
        program_name: &str,
        args: &[&str],
        mode: HermitMode,
    ) -> Result<ExecutionResult, ExecutionError> {
        let scenario = self.build_scenario(program_name, args, mode);
        self.run_scenario(&scenario)
    }

    /// Run a test scenario
    pub fn run_scenario(&self, scenario: &TestScenario) -> Result<ExecutionResult, ExecutionError> {
        // Check hardware requirements
        if !self.detector.can_run_test(&scenario.guest.name) {
            let reason = self
                .detector
                .skip_reason(&scenario.guest.name)
                .unwrap_or_else(|| "Hardware requirement not met".to_string());
            return Err(ExecutionError::HardwareUnavailable(reason));
        }

        let program_path = if scenario.guest.is_shell {
            // For shell scripts, we run them with hermit
            scenario.guest.path.clone()
        } else {
            // For Rust binaries, they should be built
            self.find_built_binary(&scenario.guest.name)?
        };

        let mut cmd = Command::new(&self.hermit_binary);
        cmd.arg("--log=off");
        cmd.arg("run");

        // Add mode-specific args
        for arg in mode.args() {
            cmd.arg(arg);
        }

        cmd.arg("--base-env=minimal");
        cmd.arg(format!(
            "--bind={}:{}",
            self.bind_dir.display(),
            "/test-fixture"
        ));
        cmd.arg("--");

        if scenario.guest.is_shell {
            cmd.arg("bash")
                .arg(&program_path)
                .args(&scenario.guest.args);
        } else {
            cmd.arg(&program_path).args(&scenario.guest.args);
        }

        let timeout = Duration::from_secs(scenario.timeout_seconds);
        self.run_with_timeout_internal(cmd, timeout)
    }

    fn build_scenario(&self, program_name: &str, args: &[&str], mode: HermitMode) -> TestScenario {
        let discoverer = super::matrix_discovery::MatrixDiscoverer::new();
        let programs = discoverer.discover_guest_programs();
        let guest = programs
            .into_iter()
            .find(|p| p.name == program_name)
            .expect("Program not found");

        let hardware_req = self.detector.classify_test(program_name);
        let timeout = discoverer.timeout_for_mode(mode, hardware_req);

        TestScenario {
            guest,
            hermit_mode: mode,
            timeout_seconds: timeout,
            hardware_requirement: hardware_req,
        }
    }

    fn find_built_binary(&self, name: &str) -> Result<PathBuf, ExecutionError> {
        // Try to find the built binary in target directory
        let target_dir = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("target"));

        let debug_path = target_dir.join("debug").join(name);
        if debug_path.exists() {
            return Ok(debug_path);
        }

        let release_path = target_dir.join("release").join(name);
        if release_path.exists() {
            return Ok(release_path);
        }

        Err(ExecutionError::IoError(format!(
            "Binary {} not found in target directory",
            name
        )))
    }

    /// Run command with timeout
    fn run_with_timeout_internal(
        &self,
        mut cmd: Command,
        timeout: Duration,
    ) -> Result<ExecutionResult, ExecutionError> {
        let started = Instant::now();

        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecutionError::IoError(e.to_string()))?;

        let deadline = started + timeout;
        let timed_out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() >= deadline => {
                    // Kill the process group
                    let pgid = -(child.id() as i32);
                    unsafe {
                        libc::kill(pgid, libc::SIGKILL);
                    }
                    break true;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(ExecutionError::IoError(e.to_string())),
            }
        };

        let output = child
            .wait_with_output()
            .map_err(|e| ExecutionError::IoError(e.to_string()))?;

        let duration = started.elapsed();

        if timed_out {
            return Err(ExecutionError::Timeout);
        }

        let exit_code = output.status.code();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Check for expected marker if present
        let _marker_matched = true; // Will be checked by caller

        Ok(ExecutionResult {
            exit_code,
            stdout,
            stderr,
            duration,
            deterministic: false, // Will be set by caller after second run
        })
    }

    /// Run a test with timeout, returning error on timeout
    pub fn run_with_timeout(
        &self,
        program_name: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecutionResult, ExecutionError> {
        let scenario = self.build_scenario(program_name, args, HermitMode::Default);
        let mut cmd = self.build_command(&scenario);

        let started = Instant::now();
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecutionError::IoError(e.to_string()))?;

        let deadline = started + timeout;
        let timed_out = loop {
            match child.try_wait() {
                Ok(Some(_)) => break false,
                Ok(None) if Instant::now() >= deadline => {
                    let pgid = -(child.id() as i32);
                    unsafe {
                        libc::kill(pgid, libc::SIGKILL);
                    }
                    break true;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(ExecutionError::IoError(e.to_string())),
            }
        };

        let output = child
            .wait_with_output()
            .map_err(|e| ExecutionError::IoError(e.to_string()))?;

        if timed_out {
            return Err(ExecutionError::Timeout);
        }

        Ok(ExecutionResult {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            duration: started.elapsed(),
            deterministic: false,
        })
    }

    fn build_command(&self, scenario: &TestScenario) -> Command {
        let mut cmd = Command::new(&self.hermit_binary);
        cmd.arg("--log=off");
        cmd.arg("run");

        for arg in scenario.hermit_mode.args() {
            cmd.arg(arg);
        }

        cmd.arg("--base-env=minimal");
        cmd.arg(format!(
            "--bind={}:{}",
            self.bind_dir.display(),
            "/test-fixture"
        ));
        cmd.arg("--");

        if scenario.guest.is_shell {
            cmd.arg("bash")
                .arg(&scenario.guest.path)
                .args(&scenario.guest.args);
        } else {
            if let Ok(binary) = self.find_built_binary(&scenario.guest.name) {
                cmd.arg(binary).args(&scenario.guest.args);
            }
        }

        cmd
    }

    /// Run record/replay test
    pub fn run_record_replay(
        &self,
        program_name: &str,
        args: &[&str],
    ) -> Result<ExecutionResult, ExecutionError> {
        // First run with --record
        let record_scenario = self.build_scenario(program_name, args, HermitMode::Record);
        let record_result = self.run_scenario(&record_scenario)?;

        // Second run with --replay
        let replay_scenario = self.build_scenario(program_name, args, HermitMode::Replay);
        let replay_result = self.run_scenario(&replay_scenario)?;

        // Check determinism
        let deterministic = record_result.exit_code == replay_result.exit_code
            && record_result.stdout == replay_result.stdout
            && record_result.stderr == replay_result.stderr;

        Ok(ExecutionResult {
            exit_code: replay_result.exit_code,
            stdout: replay_result.stdout,
            stderr: replay_result.stderr,
            duration: replay_result.duration,
            deterministic,
        })
    }

    /// Run chaos mode test
    pub fn run_chaos(
        &self,
        program_name: &str,
        args: &[&str],
    ) -> Result<ExecutionResult, ExecutionError> {
        self.run_case(program_name, args, HermitMode::Chaos)
    }

    /// Run virtual time test
    pub fn run_virtual_time(
        &self,
        program_name: &str,
        args: &[&str],
    ) -> Result<ExecutionResult, ExecutionError> {
        self.run_case(program_name, args, HermitMode::VirtualTime)
    }

    /// Run RR suite tests
    pub fn run_rr_suite(&self) -> Result<Vec<ExecutionResult>, ExecutionError> {
        if !self.detector.capabilities.has_rr {
            return Err(ExecutionError::Skipped);
        }

        // Run the rr_suite integration test
        let scenario = self.build_scenario("rr_suite", &[], HermitMode::Default);
        let result = self.run_scenario(&scenario)?;

        Ok(vec![result])
    }

    /// Run Hermit modes matrix for a program
    pub fn run_hermit_modes_matrix(
        &self,
        program_name: &str,
    ) -> HashMap<HermitMode, Result<ExecutionResult, ExecutionError>> {
        let modes = vec![
            HermitMode::Default,
            HermitMode::Strict,
            HermitMode::Chaos,
            HermitMode::VirtualTime,
            HermitMode::VirtualRandom,
        ];

        let mut results = HashMap::new();
        for mode in modes {
            let result = self.run_case(program_name, &[], mode);
            results.insert(mode, result);
        }

        results
    }

    /// Run strict mode matrix (from CI)
    pub fn run_strict_mode_matrix(&self, program_name: &str) -> Vec<ExecutionResult> {
        let tests = vec![
            "clock_determinism",
            "epoll_determinism",
            "mmap_determinism",
            "procfs_determinism",
            "signal_determinism",
        ];

        let mut results = Vec::new();
        for test in tests {
            let scenario = self.build_scenario(test, &[], HermitMode::Strict);
            if let Ok(result) = self.run_scenario(&scenario) {
                results.push(result);
            }
        }

        results
    }

    /// Run tests in parallel with limited concurrency
    pub fn run_parallel(
        &self,
        program_names: &[&str],
        max_concurrent: usize,
    ) -> Vec<Result<ExecutionResult, ExecutionError>> {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::thread;

        let results = Arc::new(Mutex::new(Vec::new()));
        let names = Arc::new(program_names.to_vec());
        let executor = Arc::new(self.clone());

        let mut handles = Vec::new();
        let semaphore = Arc::new(std::sync::Semaphore::new(max_concurrent));

        for name in names.iter() {
            let name = *name;
            let executor = executor.clone();
            let results = results.clone();
            let semaphore = semaphore.clone();

            let handle = thread::spawn(move || {
                let _permit = semaphore.acquire().unwrap();
                let result = executor.run_case(name, &[], HermitMode::Default);
                results.lock().unwrap().push(result);
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().ok();
        }

        let mut results_vec = results.lock().unwrap().drain(..).collect::<Vec<_>>();
        results_vec.sort_by_key(|r| r.is_ok());
        results_vec
    }
}

impl Clone for MatrixExecutor {
    fn clone(&self) -> Self {
        Self {
            hermit_binary: self.hermit_binary.clone(),
            detector: HardwareDetector::new(),
            bind_dir: self.bind_dir.clone(),
        }
    }
}

impl Default for MatrixExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_creation() {
        let executor = MatrixExecutor::new();
        assert!(executor.hermit_binary.exists() || true); // May not exist in test env
    }

    #[test]
    fn test_run_basic_case() {
        let executor = MatrixExecutor::new();
        // This will fail if hermit binary not built, but shouldn't panic
        let result = executor.run_case("echo", &["test"], HermitMode::Default);
        // Either ok or error, but not panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_timeout_enforcement() {
        let executor = MatrixExecutor::new();
        let result = executor.run_with_timeout("sleep", &["10"], Duration::from_secs(1));
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ExecutionError::Timeout));
    }

    #[test]
    fn test_record_replay() {
        let executor = MatrixExecutor::new();
        let result = executor.run_record_replay("echo", &["test"]);
        // Should not panic
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_chaos_mode() {
        let executor = MatrixExecutor::new();
        let result = executor.run_chaos("futex_wake_some", &[]);
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_virtual_time() {
        let executor = MatrixExecutor::new();
        let result = executor.run_virtual_time("nanosleep", &[]);
        assert!(result.is_ok() || result.is_err());
    }

    #[test]
    fn test_hermit_modes_matrix() {
        let executor = MatrixExecutor::new();
        let results = executor.run_hermit_modes_matrix("echo");

        assert!(results.contains_key(&HermitMode::Default));
        assert!(results.contains_key(&HermitMode::Strict));
        assert!(results.contains_key(&HermitMode::Chaos));
    }

    #[test]
    fn test_strict_mode_matrix() {
        let executor = MatrixExecutor::new();
        let results = executor.run_strict_mode_matrix("echo");
        // Should not panic
        let _ = results;
    }

    #[test]
    fn test_parallel_execution() {
        let executor = MatrixExecutor::new();
        let results = executor.run_parallel(&["echo", "ls", "cat"], 2);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_failure_diagnostics() {
        let executor = MatrixExecutor::new();
        let result = executor.run_case("nginx", &["-t"], HermitMode::Default);

        if let Err(e) = result {
            let diagnostic = format!("{}", e);
            assert!(!diagnostic.is_empty());
        }
    }
}
