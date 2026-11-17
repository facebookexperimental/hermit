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
    /// x86-64-v2: exactly the psABI x86-64-v2 level (the
    /// "Micro-Architecture Levels" table), needing no qualification.
    /// Fresh runs and new recordings get this table; it is the
    /// enum's default, so a serialized launch configuration without
    /// the field means it too.
    #[default]
    #[value(name = "x86-64-v2")]
    #[serde(rename = "x86-64-v2")]
    X86_64V2,
    /// The pre-psABI x86-64-v2 table (a Broadwell-era capture that
    /// also advertises AVX, BMI1, HLE, AVX2, BMI2, RTM and ADX;
    /// facebookexperimental/hermit#124), kept bit-for-bit so stored
    /// recordings replay against the CPU they recorded with. No
    /// serialized form written before the split names it: recording
    /// metadata of that era says `x86-64-v2` (or nothing) and is
    /// migrated here by recording version at load. Its public name,
    /// `x86-64-v2-legacy`, is what documents a migrated run writes
    /// carry, so a rerun presents this table by name; it may also
    /// be named explicitly to rerun what a pre-split document ran.
    /// Never recorded (recording refuses it) and never chosen by
    /// auto: resolution chooses it only for a pre-split document,
    /// and replay only for a pre-split recording, because that
    /// document's `x86-64-v2` named this table.
    #[value(name = "x86-64-v2-legacy")]
    #[serde(rename = "x86-64-v2-legacy", alias = "x86-64-v2-legacy-broadwell")]
    X86_64V2LegacyBroadwell,
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
    /// The command line's default where no profile is named. For `hermit
    /// run`: x86-64-v3 where the guest's image or its program interpreter
    /// declares it (`GNU_PROPERTY_X86_ISA_1_NEEDED`), or where the run and
    /// host qualify for it; x86-64-v2 otherwise. `hermit record start` reads
    /// the declarations: x86-64-v2 unless one declares v3, except where the
    /// glibc C library beside the program interpreter declares v3, which
    /// gets x86-64-v3 where the recording and host qualify for it, as
    /// `hermit run` does. The library's launch reads the declarations alone.
    /// Each resolves it to that named profile before qualification and
    /// launch, so a run's configuration, its manifest and a recording only
    /// ever hold the named profile, and replay uses it without deriving it
    /// again.
    #[value(name = "auto")]
    #[serde(rename = "auto")]
    Auto,
}

impl CpuProfile {
    /// Whether this profile needs no qualification and binds no host
    /// facts: both x86-64-v2 tables. (Not "the default" for launch:
    /// resolution chooses [`CpuProfile::X86_64V2`]; the legacy
    /// variant comes only from a pre-split recording or document.)
    pub fn is_default(&self) -> bool {
        matches!(
            self,
            CpuProfile::X86_64V2 | CpuProfile::X86_64V2LegacyBroadwell
        )
    }

    /// The name used on the command line, which is also the serde
    /// name: one public name per profile everywhere. The two v2
    /// tables have different names now; recordings written before
    /// the split say `x86-64-v2` (or nothing) for the legacy table
    /// and are told apart by recording version at load, not by
    /// this name.
    pub fn name(&self) -> &'static str {
        match self {
            CpuProfile::X86_64V2 => "x86-64-v2",
            CpuProfile::X86_64V2LegacyBroadwell => "x86-64-v2-legacy",
            CpuProfile::X86_64V3 => "x86-64-v3",
            CpuProfile::HostNoRdrand => "host-no-rdrand",
            CpuProfile::Auto => "auto",
        }
    }

    /// The lowest named profile presenting x86-64 level `level` (as an ISA
    /// needed property numbers them: 1 baseline, 2 v2, 3 v3, 4 v4), or `None`
    /// for a level no profile presents.
    pub fn lowest_presenting(level: u8) -> Option<CpuProfile> {
        match level {
            0..=2 => Some(CpuProfile::X86_64V2),
            3 => Some(CpuProfile::X86_64V3),
            _ => None,
        }
    }

    /// The x86-64 microarchitecture level this profile's fixed table
    /// presents (2 or 3), against which a guest image's declared need is
    /// compared. `None` for host-no-rdrand, whose features are the host's:
    /// no fixed level describes them; and for auto, which names no table
    /// until the CLI resolves it.
    pub fn x86_64_level(&self) -> Option<u8> {
        match self {
            CpuProfile::X86_64V2 | CpuProfile::X86_64V2LegacyBroadwell => Some(2),
            CpuProfile::X86_64V3 => Some(3),
            CpuProfile::HostNoRdrand | CpuProfile::Auto => None,
        }
    }
}

impl std::fmt::Display for CpuProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The facts a v3 guest's CPUID and xstate answers are built from. For a
/// host-derived machine they are captured once at startup on a host that
/// passed the qualification, and bound into a recording so that replay
/// refuses a host where any of them differs. The synthetic
/// [`HostXstate::fixed_guest`] instead describes the fixed KVM machine:
/// its vendor and signature are the presented identity, not a captured
/// host's, so a recording binding made from it does not by itself
/// identify the real host class; bind that separately before such facts
/// enter a recording or a qualification cache key.
///
/// On the ptrace backend a guest's XGETBV and XSAVE run on the host, so leaf
/// 0xD must describe the host's XCR0 and save layout; everything else in the v3
/// table is fixed. Under `fixed_guest` leaf 0xD describes the fixed
/// machine the KVM backend loads instead.
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

/// The fixed guest XCR0 the KVM backend loads: x87, SSE and YMM only.
pub const FIXED_GUEST_XCR0: u64 = 0x7;
/// The standard-format XSAVE area size for [`FIXED_GUEST_XCR0`].
pub const FIXED_GUEST_XSAVE_SIZE: u32 = 832;
/// Linux's `XFEATURE_MAX` on the 7.1.3 line: the first component index
/// `ARCH_REQ_XCOMP_PERM` answers EINVAL for (the enum ends at
/// XFEATURE_APX = 19 in `arch/x86/include/asm/fpu/types.h`; the boundary
/// was also measured directly on AMD Zen 5 / Linux 7.1.3, where indices
/// 0-19 answer EOPNOTSUPP and 20 answers EINVAL).
pub const FIXED_GUEST_XFEATURE_MAX: u32 = 20;

impl HostXstate {
    /// The synthetic facts of the fixed KVM guest machine: not read from
    /// any host, so a guest's leaf 0xD, XGETBV and arch_prctl answers are
    /// the same on every qualified host. Vendor and signature mirror the
    /// fixed table's leaf 0/1 identity words. `probe_cases` and
    /// `probe_forced_exit` describe the in-guest VM-exit qualification
    /// that admitted these facts; callers must pass the probe's real
    /// counts — the constructor cannot itself enforce that a probe ran,
    /// so admission code must not call it with invented counts.
    pub fn fixed_guest(probe_cases: u32, probe_forced_exit: bool) -> Self {
        Self {
            vendor: [0x756e_6547, 0x4965_6e69, 0x6c65_746e],
            signature: 0x0000_0663,
            xcr0: FIXED_GUEST_XCR0,
            xsave_leaves: vec![
                [
                    0,
                    FIXED_GUEST_XCR0 as u32,
                    FIXED_GUEST_XSAVE_SIZE,
                    FIXED_GUEST_XSAVE_SIZE,
                    0,
                ],
                [2, 256, 576, 0, 0],
            ],
            xcomp_supp: FIXED_GUEST_XCR0,
            xcomp_perm: FIXED_GUEST_XCR0,
            xcomp_guest_perm: FIXED_GUEST_XCR0,
            xfeature_max: FIXED_GUEST_XFEATURE_MAX,
            probe_cases,
            probe_forced_exit,
        }
    }

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
         On a host with MPX state the check covers it only with bounds checks disabled, \
         as a guest that never enables MPX runs; a guest that enables MPX itself takes \
         paths the check does not sample. \
         x86-64-v2 runs, including an auto run that chose x86-64-v2, are not affected."
    };
}

/// What the host's XSAVE results can depend on, stated wherever v3 is offered:
/// the `--cpu-profile` help, `--verify` output for an x86-64-v3 named or
/// declared, the run's manifest and log for one auto chose, the recording and
/// the docs.
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
    "The CPU the guest sees: auto (the default), x86-64-v2, x86-64-v2-legacy, x86-64-v3, or host-no-rdrand. \
     auto presents x86-64-v3 where the run and host qualify for it and x86-64-v2 everywhere \
     else; name x86-64-v2 for a run whose CPU must not depend on the host. A run qualifies \
     on the ptrace backend, with CPUID virtualized and no --gdbserver, on a host that passes \
     the x86-64-v3 startup qualification (`hermit host-capabilities --cpu-profile x86-64-v3`); \
     under a hypervisor auto presents x86-64-v2 without qualifying the host, and where a \
     check that can change between runs takes x86-64-v3 away, the run's log says so; \
     the choice is never printed on stderr, which the guest's output owns. \
     auto also reads what the guest's executable and its program interpreter declare they \
     need (GNU_PROPERTY_X86_ISA_1_NEEDED): a program declaring x86-64-v3 runs under it, \
     admitted as an explicit x86-64-v3 is, or is refused (exit 122) where it cannot run; one \
     declaring x86-64-v4, or one auto cannot read, is refused rather than guessed. \
     `hermit record start` resolves auto by those declarations, so that the recording of a \
     program that runs at x86-64-v2 does not depend on the host: x86-64-v2 unless one declares \
     x86-64-v3. The exception is a program whose glibc C library (the libc.so.6 beside its \
     program interpreter) declares x86-64-v3, which runs at x86-64-v2 on no host: record start \
     presents x86-64-v3 for it where the recording and host qualify, as `hermit run` does, and \
     x86-64-v2 elsewhere, where glibc refuses it. Hermit's library launch reads the \
     declarations alone. The chosen profile and why are logged at debug level and written to \
     the run's manifest, and a recording holds the profile; replays and reruns use it and \
     never choose again. x86-64-v2 is exactly the psABI level; recordings made before it \
     meant that replay on the legacy table they recorded with, under their stored name. \
     x86-64-v2-legacy is that legacy table's public name: a pre-split manifest or run \
     config reruns on it, documents such a run writes name it explicitly so a rerun of a \
     rerun presents the same CPU, and an explicit --cpu-profile on the command line always \
     wins over a document's era. It cannot be recorded. A later exec of an image needing more is refused where each exec is checked \
     (the ptrace backend), never upgraded; KVM checks only the first image, so a later KVM \
     exec gets no such refusal. Apart from that C library, auto reads no shared library: where \
     only another library declares its need, name the profile. \
     A named profile always means exactly itself. host-no-rdrand is available for `hermit run` on the ptrace backend with CPUID \
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
    fn the_lowest_profile_presenting_a_level() {
        for (level, profile) in [
            (0, Some(CpuProfile::X86_64V2)),
            (1, Some(CpuProfile::X86_64V2)),
            (2, Some(CpuProfile::X86_64V2)),
            (3, Some(CpuProfile::X86_64V3)),
            (4, None),
            (5, None),
        ] {
            assert_eq!(CpuProfile::lowest_presenting(level), profile, "{level}");
            if let Some(profile) = profile {
                assert!(profile.x86_64_level().unwrap() >= level.max(2));
            }
        }
        assert_eq!(CpuProfile::Auto.x86_64_level(), None);
        assert!(!CpuProfile::Auto.is_default());
    }

    #[test]
    fn fixed_guest_facts_are_the_fixed_machine() {
        let fixed = HostXstate::fixed_guest(41, true);
        assert_eq!(fixed.xcr0, FIXED_GUEST_XCR0);
        assert_eq!(fixed.xsave_leaf(0), Some([0x7, 832, 832, 0]));
        assert_eq!(fixed.xsave_leaf(1), None);
        assert_eq!(fixed.xsave_leaf(2), Some([256, 576, 0, 0]));
        assert_eq!(fixed.xsave_leaf(3), None);
        assert_eq!(fixed.xcomp_supp, 0x7);
        assert_eq!(fixed.xcomp_perm, 0x7);
        assert_eq!(fixed.xcomp_guest_perm, 0x7);
        assert_eq!(fixed.xfeature_max, 20);
        assert_eq!(fixed.probe_cases, 41);
        assert!(fixed.probe_forced_exit);
        // The identity words are the fixed table's, never a host's.
        assert_eq!(fixed.vendor, [0x756e_6547, 0x4965_6e69, 0x6c65_746e]);
        assert_eq!(fixed.signature, 0x663);
    }

    #[test]
    fn names_round_trip_on_the_command_line_and_in_serde() {
        // Every profile has one public name, on the command line
        // and in serde alike, the legacy table included: documents
        // a migrated run writes name it, so a rerun parses it back.
        let listed: Vec<&str> = CpuProfile::value_variants()
            .iter()
            .map(|profile| profile.name())
            .collect();
        assert_eq!(
            listed,
            [
                "x86-64-v2",
                "x86-64-v2-legacy",
                "x86-64-v3",
                "host-no-rdrand",
                "auto"
            ]
        );
        for profile in CpuProfile::value_variants() {
            assert_eq!(CpuProfile::from_str(profile.name(), false), Ok(*profile));
            let json = serde_json::to_string(profile).unwrap();
            assert_eq!(json, format!("\"{}\"", profile.name()));
            assert_eq!(serde_json::from_str::<CpuProfile>(&json).unwrap(), *profile);
        }
        // The interim internal spelling still deserializes, for
        // documents written while the split was in flight.
        assert_eq!(
            serde_json::from_str::<CpuProfile>("\"x86-64-v2-legacy-broadwell\"").unwrap(),
            CpuProfile::X86_64V2LegacyBroadwell
        );
        assert_eq!(
            CpuProfile::default(),
            CpuProfile::X86_64V2,
            "a fresh configuration's absent field means the psABI table"
        );
        assert!(CpuProfile::X86_64V2.is_default());
        assert!(CpuProfile::X86_64V2LegacyBroadwell.is_default());
        assert!(!CpuProfile::X86_64V3.is_default());
        assert!(!CpuProfile::HostNoRdrand.is_default());
    }
}
