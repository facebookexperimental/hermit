//! Closed host-capability vocabulary and probes shared by validation records.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize
)]
pub enum HostCapability {
    #[serde(rename = "cpuid-faulting")]
    CpuidFaulting,
    #[serde(rename = "kvm")]
    Kvm,
}

impl HostCapability {
    pub const ALL: [Self; 2] = [Self::CpuidFaulting, Self::Kvm];

    pub fn value(self) -> &'static str {
        match self {
            Self::CpuidFaulting => "cpuid-faulting",
            Self::Kvm => "kvm",
        }
    }

    pub fn from_value(text: &str) -> Option<Self> {
        match text {
            "cpuid-faulting" => Some(Self::CpuidFaulting),
            "kvm" => Some(Self::Kvm),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityVerdict {
    pub present: bool,
    pub evidence: String,
}

pub type HostCapabilities = BTreeMap<HostCapability, CapabilityVerdict>;

/// Complete machine-readable output from `hermit host-capabilities --json`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilitiesReport {
    pub schema: u64,
    pub host_capabilities: HostCapabilities,
}

impl HostCapabilitiesReport {
    pub const SCHEMA: u64 = 1;

    pub fn probe() -> Self {
        Self {
            schema: Self::SCHEMA,
            host_capabilities: probe_host_capabilities(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != Self::SCHEMA {
            return Err(format!(
                "host-capabilities schema must be {}, got {}",
                Self::SCHEMA,
                self.schema
            ));
        }
        for capability in HostCapability::ALL {
            let verdict = self
                .host_capabilities
                .get(&capability)
                .ok_or_else(|| format!("host-capabilities missing {}", capability.value()))?;
            if verdict.evidence.trim().is_empty() {
                return Err(format!(
                    "host-capabilities {} evidence must be nonempty",
                    capability.value()
                ));
            }
        }
        if self.host_capabilities.len() != HostCapability::ALL.len() {
            return Err("host-capabilities must contain the complete closed set".into());
        }
        Ok(())
    }
}

pub const ASSUME_PRESENT_ENV: &str = "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT";

pub fn probe_host_capability(capability: HostCapability) -> CapabilityVerdict {
    let forced = std::env::var(ASSUME_PRESENT_ENV).unwrap_or_default();
    if forced
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == capability.value())
    {
        return CapabilityVerdict {
            present: true,
            evidence: format!(
                "{ASSUME_PRESENT_ENV} names {}; assumed PRESENT without probing (this override can only ADD capabilities)",
                capability.value()
            ),
        };
    }
    match capability {
        HostCapability::CpuidFaulting => probe_cpuid_faulting(),
        HostCapability::Kvm => probe_kvm(),
    }
}

pub fn probe_host_capabilities() -> HostCapabilities {
    HostCapability::ALL
        .into_iter()
        .map(|capability| (capability, probe_host_capability(capability)))
        .collect()
}

pub fn cpuid_faulting_absent(syscall: Result<(), i32>, advertised: Option<bool>) -> bool {
    syscall == Err(libc::ENODEV) && advertised == Some(false)
}

fn probe_cpuid_faulting() -> CapabilityVerdict {
    let syscall = arch_prctl_set_cpuid_off();
    let advertised = cpuinfo_advertises_cpuid_fault();
    let syscall_text = match syscall {
        Ok(()) => "arch_prctl(ARCH_SET_CPUID, 0) = 0".to_string(),
        Err(0) => "arch_prctl(ARCH_SET_CPUID, 0) probe could not be completed".to_string(),
        Err(errno) => format!("arch_prctl(ARCH_SET_CPUID, 0) = -1 errno={errno}"),
    };
    let cpuinfo_text = match advertised {
        Some(true) => "/proc/cpuinfo advertises cpuid_fault",
        Some(false) => "/proc/cpuinfo does not advertise cpuid_fault",
        None => "/proc/cpuinfo could not be read",
    };
    CapabilityVerdict {
        present: !cpuid_faulting_absent(syscall, advertised),
        evidence: format!("{syscall_text}; {cpuinfo_text}"),
    }
}

fn arch_prctl_set_cpuid_off() -> Result<(), i32> {
    const ARCH_SET_CPUID: libc::c_int = 0x1012;
    // SAFETY: the child performs one syscall and `_exit`s; it never returns into Rust.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(0);
    }
    if child == 0 {
        let result = unsafe { libc::syscall(libc::SYS_arch_prctl, ARCH_SET_CPUID, 0) };
        let code = if result == 0 {
            0
        } else {
            // SAFETY: reading thread-local errno immediately after the failed syscall.
            let errno = unsafe { *libc::__errno_location() };
            errno.clamp(1, 255)
        };
        // SAFETY: this is the forked probe child and it must not run Rust destructors.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    if unsafe { libc::waitpid(child, &mut status, 0) } != child || !libc::WIFEXITED(status) {
        return Err(0);
    }
    match libc::WEXITSTATUS(status) {
        0 => Ok(()),
        errno => Err(errno),
    }
}

fn cpuinfo_advertises_cpuid_fault() -> Option<bool> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(text.split_whitespace().any(|word| word == "cpuid_fault"))
}

pub fn kvm_absent(open: Result<(), i32>, advertised: Option<bool>) -> bool {
    open == Err(libc::ENOENT) && advertised == Some(false)
}

fn probe_kvm() -> CapabilityVerdict {
    let open = open_dev_kvm();
    let advertised = cpuinfo_advertises_virtualization();
    let open_text = match open {
        Ok(()) => "open(/dev/kvm, O_RDWR) = ok".to_string(),
        Err(errno) => format!("open(/dev/kvm, O_RDWR) = -1 errno={errno}"),
    };
    let cpuinfo_text = match advertised {
        Some(true) => "/proc/cpuinfo advertises vmx or svm",
        Some(false) => "/proc/cpuinfo advertises neither vmx nor svm",
        None => "/proc/cpuinfo could not be read",
    };
    CapabilityVerdict {
        present: !kvm_absent(open, advertised),
        evidence: format!("{open_text}; {cpuinfo_text}"),
    }
}

fn open_dev_kvm() -> Result<(), i32> {
    let path = std::ffi::CString::new("/dev/kvm").expect("static path has no NUL");
    // SAFETY: `path` is valid and the descriptor is closed immediately on success.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd >= 0 {
        // SAFETY: `fd` came from the successful open above.
        unsafe { libc::close(fd) };
        return Ok(());
    }
    // SAFETY: reading thread-local errno immediately after the failed open.
    let errno = unsafe { *libc::__errno_location() };
    Err(errno.clamp(1, 255))
}

fn cpuinfo_advertises_virtualization() -> Option<bool> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    Some(
        text.split_whitespace()
            .any(|word| word == "vmx" || word == "svm"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_probe_records_every_closed_capability_with_evidence() {
        let report = HostCapabilitiesReport::probe();
        report.validate().unwrap();
        assert_eq!(report.host_capabilities.len(), HostCapability::ALL.len());
    }

    #[test]
    fn incomplete_report_refuses_by_capability_name() {
        let mut report = HostCapabilitiesReport::probe();
        report
            .host_capabilities
            .remove(&HostCapability::CpuidFaulting);
        assert_eq!(
            report.validate().unwrap_err(),
            "host-capabilities missing cpuid-faulting"
        );
    }
}
