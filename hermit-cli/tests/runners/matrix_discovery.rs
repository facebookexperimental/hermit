//! Matrix discovery - finds all guest programs and test scenarios

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// A discovered guest program with metadata
#[derive(Debug, Clone)]
pub struct GuestProgram {
    pub name: String,
    pub path: PathBuf,
    pub source: String, // "tests/", "flaky-tests/", "tests/standalone/", etc.
    pub category: String,
    pub is_rust: bool,
    pub is_shell: bool,
    pub args: Vec<String>,
    pub expected_marker: Option<String>,
}

/// Test scenario combining a guest program with Hermit mode
#[derive(Debug, Clone)]
pub struct TestScenario {
    pub guest: GuestProgram,
    pub hermit_mode: HermitMode,
    pub timeout_seconds: u64,
    pub hardware_requirement: HardwareRequirement,
}

/// Hermit execution modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HermitMode {
    Default,       // --no-virtualize-cpuid --preemption-timeout=disabled
    Strict,        // Full determinism
    Chaos,         // Chaos mode scheduling
    VirtualTime,   // Virtualized time
    VirtualRandom, // Virtualized randomness
    Record,        // Record execution
    Replay,        // Replay from trace
    Verify,        // --verify mode
}

impl HermitMode {
    pub fn args(&self) -> Vec<&'static str> {
        match self {
            HermitMode::Default => vec!["--no-virtualize-cpuid", "--preemption-timeout=disabled"],
            HermitMode::Strict => vec![],
            HermitMode::Chaos => vec!["--chaos"],
            HermitMode::VirtualTime => vec!["--virtualize-time"],
            HermitMode::VirtualRandom => vec!["--virtualize-random"],
            HermitMode::Record => vec!["--record"],
            HermitMode::Replay => vec!["--replay"],
            HermitMode::Verify => vec!["--verify"],
        }
    }

    pub fn all_modes() -> Vec<HermitMode> {
        vec![
            HermitMode::Default,
            HermitMode::Strict,
            HermitMode::Chaos,
            HermitMode::VirtualTime,
            HermitMode::VirtualRandom,
            HermitMode::Record,
            HermitMode::Replay,
            HermitMode::Verify,
        ]
    }
}

use super::hardware_detection::HardwareDetector;
use super::hardware_detection::HardwareRequirement;

/// Discovers guest programs from the tests/ and flaky-tests/ directories
pub struct MatrixDiscoverer {
    repo_root: PathBuf,
    detector: HardwareDetector,
}

impl MatrixDiscoverer {
    pub fn new() -> Self {
        let repo_root = std::env::var("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("..")
            .join("..");

        Self {
            repo_root: repo_root.canonicalize().unwrap_or(repo_root),
            detector: HardwareDetector::new(),
        }
    }

    /// Discover all guest programs from tests/ and flaky-tests/
    pub fn discover_guest_programs(&self) -> Vec<GuestProgram> {
        let mut programs = Vec::new();

        // Discover from tests/ (Rust binaries)
        programs.extend(self.discover_rust_programs("tests"));

        // Discover from flaky-tests/ (Rust binaries)
        programs.extend(self.discover_rust_programs("flaky-tests"));

        // Discover from tests/standalone/ (shell scripts)
        programs.extend(self.discover_shell_programs("tests/standalone"));

        // Discover from tests/stress/ (Rust binaries)
        programs.extend(self.discover_rust_programs("tests/stress"));

        // Discover from tests/shell/ (shell scripts)
        programs.extend(self.discover_shell_programs("tests/shell"));

        programs
    }

    fn discover_rust_programs(&self, dir: &str) -> Vec<GuestProgram> {
        let mut programs = Vec::new();
        let path = self.repo_root.join(dir);

        if !path.exists() {
            return programs;
        }

        // Read Cargo.toml to find binaries
        let cargo_toml = path.join("Cargo.toml");
        if cargo_toml.exists() {
            if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
                // Parse [[bin]] sections
                for line in content.lines() {
                    if line.trim().starts_with("name =") {
                        if let Some(name) = line.split('"').nth(1) {
                            let program = GuestProgram {
                                name: name.to_string(),
                                path: path.join(name),
                                source: dir.to_string(),
                                category: self.categorize_program(name, dir),
                                is_rust: true,
                                is_shell: false,
                                args: self.default_args_for_program(name),
                                expected_marker: self.expected_marker_for_program(name),
                            };
                            programs.push(program);
                        }
                    }
                }
            }
        }

        programs
    }

    fn discover_shell_programs(&self, dir: &str) -> Vec<GuestProgram> {
        let mut programs = Vec::new();
        let path = self.repo_root.join(dir);

        if !path.exists() {
            return programs;
        }

        if let Ok(entries) = std::fs::read_dir(&path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |ext| ext == "sh") {
                    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                        let program = GuestProgram {
                            name: name.to_string(),
                            path,
                            source: dir.to_string(),
                            category: self.categorize_program(name, dir),
                            is_rust: false,
                            is_shell: true,
                            args: Vec::new(),
                            expected_marker: None,
                        };
                        programs.push(program);
                    }
                }
            }
        }

        programs
    }

    fn categorize_program(&self, name: &str, dir: &str) -> String {
        match dir {
            "flaky-tests" => "flaky".to_string(),
            "tests/stress" => "stress".to_string(),
            "tests/standalone" => "standalone".to_string(),
            "tests/shell" => "shell".to_string(),
            _ => {
                // Categorize by name patterns
                if name.contains("determinism") || name.contains("clock") || name.contains("random")
                {
                    "determinism".to_string()
                } else if name.contains("futex")
                    || name.contains("thread")
                    || name.contains("sched")
                {
                    "threading".to_string()
                } else if name.contains("network") || name.contains("pipe") || name.contains("ipc")
                {
                    "ipc".to_string()
                } else if name.contains("mm") || name.contains("mmap") || name.contains("heap") {
                    "memory".to_string()
                } else {
                    "basic".to_string()
                }
            }
        }
    }

    fn default_args_for_program(&self, name: &str) -> Vec<String> {
        match name {
            "hello_race" | "hello_race_mini" => vec![],
            "cas_sequence_easy_bin" => vec![],
            "network_hello_world" => vec![],
            "pipe_basics" => vec![],
            "futex_and_print" => vec![],
            "futex_wake_some" => vec![],
            "futex_wait_child" => vec![],
            "futex_timeout" => vec![],
            "sched_yield" => vec![],
            "poll_spin" => vec![],
            "poll" => vec![],
            "mem_race" => vec![],
            "mem_print_race" => vec![],
            "heap_ptrs" => vec![],
            "stack_ptr" => vec![],
            "thread_random" => vec![],
            "exit_group" => vec![],
            "interrogate_tty" => vec![],
            "rdtsc" => vec![],
            "print_clock_nanosleep_monotonic_race" => vec![],
            "print_clock_nanosleep_monotonic_abs_race" => vec![],
            "print_clock_nanosleep_realtime_abs_race" => vec![],
            "print_nanosleep_race" => vec![],
            "nanosleep" => vec![],
            "socketpair" => vec![],
            _ => vec![],
        }
    }

    fn expected_marker_for_program(&self, name: &str) -> Option<String> {
        match name {
            "hello_race" | "hello_race_mini" => Some("RACE".to_string()),
            "cas_sequence_easy_bin" => Some("CAS_OK".to_string()),
            "network_hello_world" => Some("NETWORK_OK".to_string()),
            "pipe_basics" => Some("PIPE_OK".to_string()),
            "futex_and_print" => Some("FUTEX_OK".to_string()),
            "futex_wake_some" => Some("WAKE_OK".to_string()),
            "futex_wait_child" => Some("WAIT_OK".to_string()),
            "futex_timeout" => Some("TIMEOUT_OK".to_string()),
            "sched_yield" => Some("YIELD_OK".to_string()),
            "poll_spin" => Some("POLL_OK".to_string()),
            "poll" => Some("POLL_OK".to_string()),
            "mem_race" => Some("RACE_OK".to_string()),
            "mem_print_race" => Some("RACE_OK".to_string()),
            "heap_ptrs" => Some("HEAP_OK".to_string()),
            "stack_ptr" => Some("STACK_OK".to_string()),
            "thread_random" => Some("RANDOM_OK".to_string()),
            "exit_group" => Some("EXIT_OK".to_string()),
            "interrogate_tty" => Some("TTY_OK".to_string()),
            "rdtsc" => Some("RDTSC_OK".to_string()),
            "print_clock_nanosleep_monotonic_race" => Some("NANO_OK".to_string()),
            "print_clock_nanosleep_monotonic_abs_race" => Some("NANO_OK".to_string()),
            "print_clock_nanosleep_realtime_abs_race" => Some("NANO_OK".to_string()),
            "print_nanosleep_race" => Some("NANO_OK".to_string()),
            "nanosleep" => Some("NANO_OK".to_string()),
            "socketpair" => Some("SOCKET_OK".to_string()),
            _ => None,
        }
    }

    /// Generate full test matrix (guest programs × Hermit modes)
    pub fn generate_test_matrix(&self) -> Vec<TestScenario> {
        let programs = self.discover_guest_programs();
        let mut scenarios = Vec::new();

        for program in programs {
            // For each program, determine which Hermit modes are applicable
            let applicable_modes = self.applicable_modes(&program);

            for mode in applicable_modes {
                let hardware_req = self.detector.classify_test(&program.name);
                let timeout = self.timeout_for_mode(mode, hardware_req);

                scenarios.push(TestScenario {
                    guest: program.clone(),
                    hermit_mode: mode,
                    timeout_seconds: timeout,
                    hardware_requirement: hardware_req,
                });
            }
        }

        scenarios
    }

    fn applicable_modes(&self, program: &GuestProgram) -> Vec<HermitMode> {
        let mut modes = vec![HermitMode::Default, HermitMode::Strict];

        // Flaky tests are designed for chaos mode
        if program.source == "flaky-tests" {
            modes.push(HermitMode::Chaos);
        }

        // Time-related tests for virtual time
        if program.name.contains("time")
            || program.name.contains("nanosleep")
            || program.name.contains("clock")
        {
            modes.push(HermitMode::VirtualTime);
        }

        // Random-related tests for virtual random
        if program.name.contains("random")
            || program.name.contains("rdrand")
            || program.name.contains("rdseed")
        {
            modes.push(HermitMode::VirtualRandom);
        }

        // Record/replay for all programs that can record
        if !program.is_shell {
            modes.push(HermitMode::Record);
            modes.push(HermitMode::Replay);
        }

        // Verify mode for standalone tests
        if program.source == "tests/standalone" {
            modes.push(HermitMode::Verify);
        }

        modes
    }

    fn timeout_for_mode(&self, mode: HermitMode, hardware: HardwareRequirement) -> u64 {
        let base = match mode {
            HermitMode::Default => 30,
            HermitMode::Strict => 60,
            HermitMode::Chaos => 60,
            HermitMode::VirtualTime => 30,
            HermitMode::VirtualRandom => 30,
            HermitMode::Record => 60,
            HermitMode::Replay => 60,
            HermitMode::Verify => 120,
        };

        // Hardware-dependent tests get longer timeout
        if hardware != HardwareRequirement::None {
            base * 2
        } else {
            base
        }
    }

    /// Generate coverage manifest mapping Buck scenarios to ported status
    pub fn generate_coverage_manifest(&self) -> String {
        let programs = self.discover_guest_programs();
        let mut manifest = String::new();

        manifest.push_str("# Hermit Integration Test Coverage Manifest\n");
        manifest.push_str("# Maps internal Buck scenarios to Cargo-native port status\n\n");
        manifest.push_str("buck_scenario\tcargo_test\tstatus\thardware_req\tnotes\n");

        for program in programs {
            let modes = self.applicable_modes(&program);
            for mode in modes {
                let cargo_test = format!("{}::{}", program.name, mode_name(mode));
                let status = if self.detector.can_run_test(&program.name) {
                    "ported"
                } else {
                    "pending_hardware"
                };
                let hw_req = format!("{:?}", self.detector.classify_test(&program.name));

                manifest.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    format!("hermit/{}/{}", program.source, program.name),
                    cargo_test,
                    status,
                    hw_req,
                    ""
                ));
            }
        }

        // Add known internal-only scenarios
        manifest.push_str("\n# Internal-only scenarios (Meta infrastructure)\n");
        manifest.push_str("rr/full_suite\trr_suite\texcluded\trr\tRequires rr recordings\n");
        manifest.push_str("qemu/l2_boot\tqemu_l2\texcluded\tkvm\tRequires QEMU + kernel\n");

        manifest
    }
}

fn mode_name(mode: HermitMode) -> &'static str {
    match mode {
        HermitMode::Default => "default",
        HermitMode::Strict => "strict",
        HermitMode::Chaos => "chaos",
        HermitMode::VirtualTime => "virtual_time",
        HermitMode::VirtualRandom => "virtual_random",
        HermitMode::Record => "record",
        HermitMode::Replay => "replay",
        HermitMode::Verify => "verify",
    }
}

impl Default for MatrixDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discoverer_creation() {
        let discoverer = MatrixDiscoverer::new();
        assert!(discoverer.repo_root.exists());
    }

    #[test]
    fn test_discover_guest_programs() {
        let discoverer = MatrixDiscoverer::new();
        let programs = discoverer.discover_guest_programs();
        assert!(!programs.is_empty());
    }

    #[test]
    fn test_discovery_includes_tests_and_flaky_tests() {
        let discoverer = MatrixDiscoverer::new();
        let programs = discoverer.discover_guest_programs();

        let has_tests = programs.iter().any(|p| p.source.contains("tests/"));
        let has_flaky_tests = programs.iter().any(|p| p.source.contains("flaky-tests/"));

        assert!(has_tests);
        assert!(has_flaky_tests);
    }

    #[test]
    fn test_generate_test_matrix() {
        let discoverer = MatrixDiscoverer::new();
        let matrix = discoverer.generate_test_matrix();
        assert!(!matrix.is_empty());
    }

    #[test]
    fn test_applicable_modes_for_flaky() {
        let discoverer = MatrixDiscoverer::new();
        let programs = discoverer.discover_guest_programs();

        let flaky = programs.iter().find(|p| p.source == "flaky-tests");
        if let Some(p) = flaky {
            let modes = discoverer.applicable_modes(p);
            assert!(modes.contains(&HermitMode::Chaos));
        }
    }

    #[test]
    fn test_coverage_manifest_generation() {
        let discoverer = MatrixDiscoverer::new();
        let manifest = discoverer.generate_coverage_manifest();

        assert!(manifest.contains("buck_scenario"));
        assert!(manifest.contains("cargo_test"));
        assert!(manifest.contains("status"));
        assert!(manifest.contains("hardware_req"));
    }

    #[test]
    fn test_timeout_for_modes() {
        let discoverer = MatrixDiscoverer::new();

        assert_eq!(
            discoverer.timeout_for_mode(HermitMode::Default, HardwareRequirement::None),
            30
        );
        assert_eq!(
            discoverer.timeout_for_mode(HermitMode::Strict, HardwareRequirement::None),
            60
        );
        assert_eq!(
            discoverer.timeout_for_mode(HermitMode::Chaos, HardwareRequirement::None),
            60
        );
        assert_eq!(
            discoverer.timeout_for_mode(HermitMode::Verify, HardwareRequirement::None),
            120
        );

        // Hardware tests get double timeout
        assert_eq!(
            discoverer.timeout_for_mode(HermitMode::Default, HardwareRequirement::Pmu),
            60
        );
    }
}
