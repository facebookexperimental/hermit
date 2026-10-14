/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::Write;

use clap::Args;
use detcore_model::host_capability::CapabilityVerdict;
use detcore_model::host_capability::HostCapabilitiesReport;
use hermit::Error;
use hermit::ExitStatus;
use reverie_ptrace::PmuValidationError;

#[derive(Debug, Args)]
pub struct HostCapabilitiesOpts {
    /// Emit the complete producer-owned capability record as one JSON object.
    #[clap(long)]
    json: bool,
}

impl HostCapabilitiesOpts {
    pub fn main(&self) -> Result<ExitStatus, Error> {
        let report = HostCapabilitiesReport::probe(exact_branch_counter_verdict());
        report.validate().map_err(Error::msg)?;
        let mut stdout = std::io::stdout().lock();
        if self.json {
            serde_json::to_writer(&mut stdout, &report)?;
            writeln!(stdout)?;
        } else {
            for (capability, verdict) in report.host_capabilities {
                writeln!(
                    stdout,
                    "{}: {} — {}",
                    capability.value(),
                    if verdict.present { "PRESENT" } else { "ABSENT" },
                    verdict.evidence
                )?;
            }
            writeln!(
                stdout,
                "exact-branch-counter: {} — {}",
                if report.exact_branch_counter.present {
                    "PRESENT"
                } else {
                    "ABSENT"
                },
                report.exact_branch_counter.evidence
            )?;
        }
        Ok(ExitStatus::Exited(0))
    }
}

/// Why this host's retired-conditional-branch counter cannot drive an exact
/// virtual clock, or `None` when it can or when no counter is in use.
///
/// Hermit's virtual clock and preemption points on the ptrace and e9patch
/// backends come from that counter, so a counter that miscounts makes two runs
/// of one program diverge. Reverie validates the counters once per process;
/// `validation` returns its verdict. Without perf support Reverie arms no
/// counter, so there is nothing to validate.
pub(crate) fn inexact_branch_counter(
    perf_supported: bool,
    validation: impl FnOnce() -> Result<(), String>,
) -> Option<String> {
    if !perf_supported {
        return None;
    }
    validation().err()
}

/// [`inexact_branch_counter`] for this host, using Reverie's validation, with
/// Reverie's full message, measured counts included.
pub(crate) fn host_inexact_branch_counter() -> Option<String> {
    inexact_branch_counter(reverie_ptrace::is_perf_supported(), || {
        reverie_ptrace::pmu_validation().map_err(|error| error.to_string())
    })
}

/// The kind of validation failure, without the measured counts that Reverie's
/// message carries. Those differ from one process to the next (the SpecLockMap
/// check counted 91 to 2,374 commits on one AMD EPYC 9D64), while two probes of
/// one host, such as the Cargo- and Buck-built binaries that the release check
/// compares, must report the same verdict. The error is non-exhaustive, so the
/// kind is the variant name that its `Debug` output starts with.
fn validation_failure_kind(error: &PmuValidationError) -> String {
    format!("{error:?}")
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// The `exact_branch_counter` verdict that `hermit host-capabilities` reports.
/// It is absent exactly when `hermit run --strict` refuses the ptrace and
/// e9patch backends with a timeslice, which names the counts.
pub(crate) fn exact_branch_counter_verdict() -> CapabilityVerdict {
    let cpu = cpu_identity();
    let failure = inexact_branch_counter(reverie_ptrace::is_perf_supported(), || {
        reverie_ptrace::pmu_validation().map_err(validation_failure_kind)
    });
    match failure {
        Some(kind) => CapabilityVerdict {
            present: false,
            evidence: format!(
                "{cpu}: Reverie performance-counter validation failed ({kind}); \
                 `hermit run --strict` names the measured counts"
            ),
        },
        None if reverie_ptrace::is_perf_supported() => CapabilityVerdict {
            present: true,
            evidence: format!("{cpu}: Reverie performance-counter validation passed"),
        },
        None => CapabilityVerdict {
            present: true,
            evidence: format!(
                "{cpu}: perf hardware events are unavailable, so Reverie arms no branch counter"
            ),
        },
    }
}

/// The CPU vendor, family, model and microcode from `/proc/cpuinfo`, which is
/// what ties a counter erratum to a CPU model.
pub(crate) fn cpu_identity() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|text| cpu_identity_from(&text))
        .unwrap_or_else(|error| format!("CPU unknown (/proc/cpuinfo: {error})"))
}

fn cpu_identity_from(cpuinfo: &str) -> String {
    let first_processor = cpuinfo.split("\n\n").next().unwrap_or_default();
    let field = |name: &str| {
        first_processor
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.trim() == name)
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    format!(
        "CPU {} ({} family {} model {} microcode {})",
        field("model name"),
        field("vendor_id"),
        field("cpu family"),
        field("model"),
        field("microcode")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_failed_validation_of_an_armed_counter_is_inexact() {
        assert_eq!(
            inexact_branch_counter(true, || Err("SpecLockMap is enabled".into())),
            Some("SpecLockMap is enabled".into())
        );
        assert_eq!(inexact_branch_counter(true, || Ok(())), None);
        assert_eq!(
            inexact_branch_counter(false, || panic!("no counter, nothing to validate")),
            None
        );
    }

    /// Two failures that differ only in a measured count are one kind, so two
    /// probes of one host report the same verdict.
    #[test]
    fn a_validation_failure_kind_carries_no_measured_count() {
        let failure = |actual_events| PmuValidationError::HardwareCountersNotWorking {
            actual_events,
            expected_min_events: 500,
            config: 0x5101c4,
        };
        assert_ne!(failure(91).to_string(), failure(2374).to_string());
        assert_eq!(
            validation_failure_kind(&failure(91)),
            "HardwareCountersNotWorking"
        );
        assert_eq!(
            validation_failure_kind(&failure(91)),
            validation_failure_kind(&failure(2374))
        );
        assert_eq!(
            validation_failure_kind(&PmuValidationError::IocPeriodBugDetected),
            "IocPeriodBugDetected"
        );
    }

    #[test]
    fn cpu_identity_reads_the_first_processor() {
        let cpuinfo = "processor\t: 0\nvendor_id\t: AuthenticAMD\ncpu family\t: 25\n\
                       model\t\t: 160\nmodel name\t: AMD EPYC 9D64 88-Core Processor\n\
                       microcode\t: 0xaa0021c\n\nprocessor\t: 1\nvendor_id\t: Other\n";
        assert_eq!(
            cpu_identity_from(cpuinfo),
            "CPU AMD EPYC 9D64 88-Core Processor (AuthenticAMD family 25 model 160 microcode \
             0xaa0021c)"
        );
        assert_eq!(
            cpu_identity_from(""),
            "CPU unknown (unknown family unknown model unknown microcode unknown)"
        );
    }
}
