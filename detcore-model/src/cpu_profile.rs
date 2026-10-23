/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The virtual CPU a guest sees (<https://github.com/rrnewton/hermit/issues/4243>).

use serde::Deserialize;
use serde::Serialize;

/// Which intercepted CPUID answers the guest receives.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum
)]
pub enum CpuProfile {
    /// x86-64-v2, the default: the table every earlier Hermit has served.
    #[default]
    #[value(name = "x86-64-v2")]
    #[serde(rename = "x86-64-v2")]
    X86_64V2,
    /// x86-64-v3: v2 plus AVX2, BMI1/2, FMA, F16C, LZCNT, MOVBE and XSAVE.
    /// Offered only on the ptrace backend, on hosts that pass its startup
    /// qualification.
    #[value(name = "x86-64-v3")]
    #[serde(rename = "x86-64-v3")]
    X86_64V3,
    /// Host CPUID with hardware-randomness and unvirtualized timing features
    /// hidden. Requires CPUID faulting and virtual time on the ptrace backend.
    #[value(name = "host-no-rdrand")]
    #[serde(rename = "host-no-rdrand")]
    HostNoRdrand,
}

impl CpuProfile {
    /// Whether this is the default profile, which needs no qualification and
    /// binds no host facts.
    pub fn is_default(&self) -> bool {
        *self == CpuProfile::X86_64V2
    }

    /// The name used on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            CpuProfile::X86_64V2 => "x86-64-v2",
            CpuProfile::X86_64V3 => "x86-64-v3",
            CpuProfile::HostNoRdrand => "host-no-rdrand",
        }
    }
}

impl std::fmt::Display for CpuProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The host facts a v3 guest's CPUID and xstate answers are built from,
/// captured once at startup on a host that passed the qualification, and bound
/// into a recording so that replay refuses a host where any of them differs.
///
/// On the ptrace backend a guest's XGETBV and XSAVE run on the host, so leaf
/// 0xD must describe the host's XCR0 and save layout; everything else in the v3
/// table is fixed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostXstate {
    /// CPUID leaf 0 EBX, EDX, ECX: the vendor string.
    pub vendor: [u32; 3],
    /// CPUID leaf 1 EAX: family, model and stepping.
    pub signature: u32,
    /// XCR0, as XGETBV(0) reads it.
    pub xcr0: u64,
    /// The host's CPUID leaf 0xD rows the guest table is built from, as
    /// `[subleaf, eax, ebx, ecx, edx]`: subleaf 0 and each enabled component
    /// from 2 up, in order.
    pub xsave_leaves: Vec<[u32; 5]>,
    /// `arch_prctl` ARCH_GET_XCOMP_SUPP, ARCH_GET_XCOMP_PERM and
    /// ARCH_GET_XCOMP_GUEST_PERM.
    pub xcomp_supp: u64,
    pub xcomp_perm: u64,
    pub xcomp_guest_perm: u64,
    /// The first component index ARCH_REQ_XCOMP_PERM answers EINVAL for
    /// (Linux's XFEATURE_MAX); below it the answer is EOPNOTSUPP.
    pub xfeature_max: u32,
    /// The qualification this host passed: its case count and whether the
    /// forced-exit arm ran.
    pub probe_cases: u32,
    pub probe_forced_exit: bool,
}

impl HostXstate {
    /// The host's leaf 0xD row for `subleaf`, if it was captured.
    pub fn xsave_leaf(&self, subleaf: u32) -> Option<[u32; 4]> {
        self.xsave_leaves
            .iter()
            .find(|row| row[0] == subleaf)
            .map(|row| [row[1], row[2], row[3], row[4]])
    }
}

/// The residual as a literal, so that the command-line help can include it
/// with `concat!` while [`V3_RESIDUAL`] stays the one source of its words.
#[macro_export]
macro_rules! v3_residual {
    () => {
        "With --cpu-profile x86-64-v3, Hermit checks at startup that this host does not \
         change the XSTATE_BV bits a guest reads (with XSAVE, XSAVEC or XSAVEOPT, or in a \
         signal frame) when the host saves and restores the guest's registers. The check \
         samples a fixed set of cases and forces only CPUID exits, so a CPU or hypervisor \
         that behaves differently only on other instruction paths, or on VM exits the guest \
         cannot see, is not excluded. Firmware (SMM) entries cannot be checked at all. \
         Default runs use x86-64-v2, which this does not affect."
    };
}

/// What the host's XSAVE results can depend on, stated wherever v3 is offered:
/// the `--cpu-profile` help, `--verify` output, the recording and the docs.
pub const V3_RESIDUAL: &str = v3_residual!();

/// The host-profile caveat, shared by help and admission diagnostics.
#[macro_export]
macro_rules! host_no_rdrand_residual {
    () => {
        "With --cpu-profile host-no-rdrand, CPUID remains intercepted, but its answers \
         come from the host CPU with hardware-randomness and unvirtualized timing \
         features hidden. Results are host-CPU-model specific: cross-host builds need \
         the same CPU model and compatible kernel/XSTATE settings and CPU topology. \
         The tracer and guest are pinned to the lowest originally allowed CPU, even \
         with the timeslice disabled; repeat runs require the same allowed CPU set. Host XSAVE behavior \
         is retained without the x86-64-v3 XSTATE qualification, so programs inspecting \
         XSAVE or signal-register metadata have no determinism guarantee. Programs must honor \
         the advertised feature bits; unconditional RDRAND or RDSEED is not trapped \
         by this profile."
    };
}

pub const HOST_NO_RDRAND_RESIDUAL: &str = host_no_rdrand_residual!();

/// The long help of `--cpu-profile`, for `hermit run` and `hermit record start`.
pub const CPU_PROFILE_LONG_HELP: &str = concat!(
    "The CPU the guest sees: x86-64-v2 (the default), x86-64-v3, or host-no-rdrand. \
     host-no-rdrand is available for `hermit run` on the ptrace backend with CPUID \
     faulting and virtual time; it refuses hosts without CPUID faulting instead of \
     allowing native CPUID. RDRAND and RDSEED are hidden. Check availability with \
     `hermit host-capabilities --cpu-profile host-no-rdrand`. Recording and replay \
     with host-no-rdrand are not supported.\n\n",
    host_no_rdrand_residual!(),
    "\n\n",
    "v3 is offered only on \
     the ptrace backend, on hosts that pass a startup qualification \
     (`hermit host-capabilities --cpu-profile x86-64-v3` runs it alone). A recording binds \
     the host facts its guest's answers come from, and replay refuses a host where any of \
     them differs (<https://github.com/rrnewton/hermit/issues/4243>).\n\n",
    v3_residual!()
);

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

    use super::*;

    #[test]
    fn names_round_trip_on_the_command_line_and_in_serde() {
        for profile in CpuProfile::value_variants() {
            assert_eq!(CpuProfile::from_str(profile.name(), false), Ok(*profile));
            let json = serde_json::to_string(profile).unwrap();
            assert_eq!(json, format!("\"{}\"", profile.name()));
            assert_eq!(serde_json::from_str::<CpuProfile>(&json).unwrap(), *profile);
        }
        assert_eq!(CpuProfile::default(), CpuProfile::X86_64V2);
        assert!(CpuProfile::X86_64V2.is_default());
        assert!(!CpuProfile::X86_64V3.is_default());
        assert!(!CpuProfile::HostNoRdrand.is_default());
    }
}
