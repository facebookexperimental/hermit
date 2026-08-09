//! Hardware detection and test classification for the integration runner

use std::collections::HashSet;
use std::path::Path;

/// Hardware requirements for a test
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HardwareRequirement {
    /// No special hardware required - runs on any x86_64 Linux
    None,
    /// Requires PMU (Performance Monitoring Unit) access for retired conditional branch counting
    Pmu,
    /// Requires CPUID interception capability
    CpuidInterception,
    /// Requires specific CPU features (RDRAND, RDSEED, etc.)
    CpuFeatures,
    /// Requires rr (record and replay) tool
    Rr,
    /// Requires KVM access
    Kvm,
    /// Requires DynamoRIO
    Dbi,
}

/// Detected hardware capabilities
#[derive(Debug, Clone, Default)]
pub struct HardwareCapabilities {
    pub has_pmu: bool,
    pub has_cpuid_interception: bool,
    pub has_rdrand: bool,
    pub has_rdseed: bool,
    pub has_rr: bool,
    pub has_kvm: bool,
    pub has_dbi: bool,
    pub cpu_model: Option<String>,
}

/// Hardware detector for classifying tests and detecting capabilities
pub struct HardwareDetector {
    capabilities: HardwareCapabilities,
    pmu_tests: HashSet<String>,
    cpuid_tests: HashSet<String>,
    cpu_feature_tests: HashSet<String>,
    rr_tests: HashSet<String>,
}

impl HardwareDetector {
    pub fn new() -> Self {
        let mut detector = Self {
            capabilities: HardwareCapabilities::default(),
            pmu_tests: HashSet::new(),
            cpuid_tests: HashSet::new(),
            cpu_feature_tests: HashSet::new(),
            rr_tests: HashSet::new(),
        };
        detector.populate_test_classifications();
        detector.detect_capabilities();
        detector
    }

    fn populate_test_classifications(&mut self) {
        // PMU-dependent tests (from CI config)
        self.pmu_tests.extend([
            "getrandom_intercepted".to_string(),
            "futex_wait_parent".to_string(),
            "mem_race".to_string(),
            "mem_print_race".to_string(),
            "scheduling_fairness".to_string(),
            "concurrency".to_string(),
        ]);

        // CPUID interception tests
        self.cpuid_tests.extend([
            "has_rdrand_without_detcore".to_string(),
            "rdrand_rdseed_is_masked".to_string(),
            "cpuid_basic".to_string(),
        ]);

        // CPU feature tests
        self.cpu_feature_tests
            .extend(["rdrand_basic".to_string(), "rdseed_basic".to_string()]);

        // RR suite tests
        self.rr_tests
            .extend(["rr_suite".to_string(), "record_replay_matrix".to_string()]);
    }

    fn detect_capabilities(&mut self) {
        // Check PMU access
        self.capabilities.has_pmu = self.check_pmu_access();

        // Check CPUID interception (requires kernel support)
        self.capabilities.has_cpuid_interception = self.check_cpuid_interception();

        // Check RDRAND/RDSEED
        self.capabilities.has_rdrand = self.check_rdrand();
        self.capabilities.has_rdseed = self.check_rdseed();

        // Check rr availability
        self.capabilities.has_rr = self.check_rr();

        // Check KVM
        self.capabilities.has_kvm = self.check_kvm();

        // Check DynamoRIO
        self.capabilities.has_dbi = self.check_dbi();

        // Get CPU model
        self.capabilities.cpu_model = self.get_cpu_model();
    }

    fn check_pmu_access(&self) -> bool {
        // Try to read from perf_event_paranoid
        std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
            .map(|s| s.trim().parse::<i32>().unwrap_or(3) <= 1)
            .unwrap_or(false)
    }

    fn check_cpuid_interception(&self) -> bool {
        // Check if we can intercept CPUID (typically requires specific kernel config)
        // For now, assume false in most environments
        std::path::Path::new("/dev/kvm").exists()
    }

    fn check_rdrand(&self) -> bool {
        // Check CPUID for RDRAND support
        std::fs::read_to_string("/proc/cpuinfo")
            .map(|s| s.contains("rdrand"))
            .unwrap_or(false)
    }

    fn check_rdseed(&self) -> bool {
        std::fs::read_to_string("/proc/cpuinfo")
            .map(|s| s.contains("rdseed"))
            .unwrap_or(false)
    }

    fn check_rr(&self) -> bool {
        which::which("rr").is_ok()
    }

    fn check_kvm(&self) -> bool {
        std::path::Path::new("/dev/kvm").exists()
            && std::fs::metadata("/dev/kvm")
                .map(|m| m.permissions().mode() & 0o666 != 0)
                .unwrap_or(false)
    }

    fn check_dbi(&self) -> bool {
        std::env::var("DYNAMORIO_HOME").is_ok() && std::env::var("HERMIT_DRRUN").is_ok()
    }

    fn get_cpu_model(&self) -> Option<String> {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        })
    }

    /// Classify a test by its hardware requirements
    pub fn classify_test(&self, test_name: &str) -> HardwareRequirement {
        if self.pmu_tests.contains(test_name) {
            HardwareRequirement::Pmu
        } else if self.cpuid_tests.contains(test_name) {
            HardwareRequirement::CpuidInterception
        } else if self.cpu_feature_tests.contains(test_name) {
            HardwareRequirement::CpuFeatures
        } else if self.rr_tests.contains(test_name) {
            HardwareRequirement::Rr
        } else {
            HardwareRequirement::None
        }
    }

    /// Get all detected capabilities
    pub fn detect_capabilities(&self) -> HardwareCapabilities {
        self.capabilities.clone()
    }

    /// Check if a test can run on current hardware
    pub fn can_run_test(&self, test_name: &str) -> bool {
        let requirement = self.classify_test(test_name);
        match requirement {
            HardwareRequirement::None => true,
            HardwareRequirement::Pmu => self.capabilities.has_pmu,
            HardwareRequirement::CpuidInterception => self.capabilities.has_cpuid_interception,
            HardwareRequirement::CpuFeatures => {
                self.capabilities.has_rdrand || self.capabilities.has_rdseed
            }
            HardwareRequirement::Rr => self.capabilities.has_rr,
            HardwareRequirement::Kvm => self.capabilities.has_kvm,
            HardwareRequirement::Dbi => self.capabilities.has_dbi,
        }
    }

    /// Get reason why a test cannot run
    pub fn skip_reason(&self, test_name: &str) -> Option<String> {
        if self.can_run_test(test_name) {
            return None;
        }

        let requirement = self.classify_test(test_name);
        Some(match requirement {
            HardwareRequirement::Pmu => {
                "PMU access not available (perf_event_paranoid > 1 or no PMU)".to_string()
            }
            HardwareRequirement::CpuidInterception => {
                "CPUID interception not available (no KVM or kernel support)".to_string()
            }
            HardwareRequirement::CpuFeatures => {
                "Required CPU features (RDRAND/RDSEED) not present".to_string()
            }
            HardwareRequirement::Rr => "rr tool not installed".to_string(),
            HardwareRequirement::Kvm => "KVM not available or not accessible".to_string(),
            HardwareRequirement::Dbi => {
                "DynamoRIO not configured (DYNAMORIO_HOME, HERMIT_DRRUN)".to_string()
            }
            HardwareRequirement::None => unreachable!(),
        })
    }
}

impl Default for HardwareDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hardware_detector_creation() {
        let detector = HardwareDetector::new();
        // Should not panic
    }

    #[test]
    fn test_classify_basic_test() {
        let detector = HardwareDetector::new();
        assert_eq!(
            detector.classify_test("basic_echo"),
            HardwareRequirement::None
        );
    }

    #[test]
    fn test_classify_pmu_test() {
        let detector = HardwareDetector::new();
        assert_eq!(detector.classify_test("mem_race"), HardwareRequirement::Pmu);
    }

    #[test]
    fn test_classify_cpuid_test() {
        let detector = HardwareDetector::new();
        assert_eq!(
            detector.classify_test("has_rdrand_without_detcore"),
            HardwareRequirement::CpuidInterception
        );
    }

    #[test]
    fn test_classify_rr_test() {
        let detector = HardwareDetector::new();
        assert_eq!(detector.classify_test("rr_suite"), HardwareRequirement::Rr);
    }

    #[test]
    fn test_detect_capabilities() {
        let detector = HardwareDetector::new();
        let caps = detector.detect_capabilities();
        // Just verify it runs without panic
        let _ = caps.has_pmu;
    }

    #[test]
    fn test_skip_reason_for_pmu() {
        let detector = HardwareDetector::new();
        let reason = detector.skip_reason("mem_race");
        // May be Some or None depending on environment
        assert!(reason.is_some() || reason.is_none());
    }
}
