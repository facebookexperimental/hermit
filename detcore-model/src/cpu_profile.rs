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

/// Which x86-64 microarchitecture level the virtual CPUID table describes.
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
        }
    }
}

impl std::fmt::Display for CpuProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What the host's XSAVE results can depend on, stated wherever v3 is offered:
/// the `--cpu-profile` help, `--verify` output, the recording and the docs.
pub const V3_RESIDUAL: &str = "With --cpu-profile x86-64-v3, Hermit checks at startup that this \
host does not change the XSTATE_BV bits a guest reads (with XSAVE, XSAVEC or XSAVEOPT, or in a \
signal frame) when the host saves and restores the guest's registers. The check samples a fixed \
set of cases and forces only CPUID exits, so a CPU or hypervisor that behaves differently only on \
other instruction paths, or on VM exits the guest cannot see, is not excluded. Firmware (SMM) \
entries cannot be checked at all. Default runs use x86-64-v2, which this does not affect.";

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
    }
}
