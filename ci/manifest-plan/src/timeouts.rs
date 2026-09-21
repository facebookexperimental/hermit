// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.

// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

pub const MANIFEST_SCHEMA: u64 = 3;
pub const DEFAULTS_FILE: &str = "defaults.yaml";
pub const MIN_TIMEOUT_SECONDS: u64 = 1;
pub const MAX_TIMEOUT_SECONDS: u64 = 1800;
/// Owner-approved ordinary-cell CPU bound: ceil(1.5 * 14.298 s) = 22 s.
pub const DEFAULT_TEST_CPU_TIMEOUT_SECONDS: u64 = 22;
/// Owner-approved ordinary-test wall bound: ceil(4 * 14.019 s) = 57 s.
pub const DEFAULT_TEST_WALL_TIMEOUT_SECONDS: u64 = 57;
pub const TEST_CPU_TIMEOUT_MULTIPLIER_ENV: &str = "HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER";
pub const TEST_WALL_TIMEOUT_MULTIPLIER_ENV: &str = "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER";

/// Frozen retained-data census used to calibrate the per-test policy.
pub const TIMEOUT_CALIBRATION_CUTOFF_UTC: &str = "2026-09-03T02:18:30Z";
pub const CALIBRATED_CI_CELL_COUNT: usize = 492;
pub const DEFAULT_COVERED_CI_CELL_COUNT: usize = 487;
pub const NON_CI_CELL_COUNT: usize = 177;
/// Additional selected cells covered by the KVM qualification evidence.
pub const KVM_RATCHET_CALIBRATION_SHA: &str = "92bacf12deba6a717f77cfcbd6afefc5ffb383f2";
pub const KVM_RATCHET_CALIBRATION_COMPLETED_UTC: &str = "2026-09-04T04:39:00Z";
pub const KVM_RATCHET_CI_CELL_COUNT: usize = 183;
pub const KVM_RATCHET_DEFAULT_COVERED_CI_CELL_COUNT: usize = 182;
pub const KVM_TIMED_PROGRESS_BAR_REQUALIFICATION_SHA: &str =
    "f190205a7b3e65e7ebf347ba0875b668fd72d9a7";
pub const KVM_TIMED_PROGRESS_BAR_REQUALIFICATION_COMPLETED_UTC: &str = "2026-09-05T03:43:05Z";
/// Cells removed from full selection after RUN1709 did not pass on the first attempt.
pub const KVM_RUN_1709_CI_REMOVAL_COUNT: usize = 10;
/// Cells selected after each passed three first-attempt canonical KVM L2 runs
/// with zero retries in the pinned glibc 2.42 validation image.
pub const KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT: usize = 15;
/// Further KVM cells selected after three first-attempt canonical L2 passes
/// with zero retries in the pinned glibc 2.42 validation image.
pub const KVM_NEXT40_QUALIFICATION_SHA: &str = "4d8f866102882b6eeabdf99cc7e81433cc3c95c5";
pub const KVM_NEXT40_QUALIFICATION_COMPLETED_UTC: &str = "2026-09-04T20:26:39Z";
pub const KVM_NEXT40_QUALIFIED_CI_CELL_COUNT: usize = 30;
pub const KVM_NEXT40_DEFAULT_COVERED_MAX_REQUIRED_CPU_SECONDS: u64 = 3;
pub const KVM_NEXT40_DEFAULT_COVERED_MAX_REQUIRED_WALL_SECONDS: u64 = 7;
/// Further KVM cells selected after three first-attempt strict verification
/// passes with no retries in the pinned glibc 2.42 validation image.
pub const KVM_2026_09_08_EVIDENCE_SHA: &str = "9d4bb692ddfe02241e6341a2faeb83782215e1a5";
pub const KVM_2026_09_08_EVIDENCE_COMPLETED_UTC: &str = "2026-09-08T14:03:16Z";
pub const KVM_2026_09_08_SELECTED_CI_CELL_COUNT: usize = 21;
pub const KVM_2026_09_08_MAX_REQUIRED_CPU_SECONDS: u64 = 7;
pub const KVM_2026_09_08_MAX_REQUIRED_WALL_SECONDS: u64 = 20;
/// One ptrace chaos cell promoted after re-measuring its observation diversity
/// against its own UNCHANGED floor of `assert.min_distinct: 16`.
///
/// `c-programs/ipc-determinism` chaos/ptrace was held because a 2026-08-25 run
/// saw only 6 distinct observation classes. Re-measurement contradicts that: the
/// cell's 32 seeds produce 32 distinct status-plus-stdout classes, twice the
/// floor, with 32 of 32 same-seed strict comparisons matching. The floor was not
/// lowered; a control run with the floor raised to 33 fails, so the assertion is
/// live rather than decorative.
pub const IPC_DETERMINISM_CHAOS_EVIDENCE_SHA: &str = "0b26fb782192e017ef9103e27d017f8c73ceeeb4";
pub const IPC_DETERMINISM_CHAOS_EVIDENCE_COMPLETED_UTC: &str = "2026-09-15T22:07:28Z";
pub const IPC_DETERMINISM_CHAOS_SELECTED_CI_CELL_COUNT: usize = 1;
/// LiteInst host-hybrid cells selected after ten clean first-attempt strict
/// verification repetitions each. Keep this evidence separate from the frozen
/// census and the KVM qualifications; the ordinary 22/57 bounds are unchanged.
pub const LITEINST_2026_09_16_EVIDENCE_SHA: &str = "ed99e05133b00058fafed7f3dfaf48ab8fd334e6";
pub const LITEINST_2026_09_16_EVIDENCE_COMPLETED_UTC: &str = "2026-09-16T22:31:37Z";
pub const LITEINST_2026_09_16_SELECTED_CI_CELL_COUNT: usize =
    LITEINST_2026_09_16_TIMEOUT_CALIBRATIONS.len();
/// Previously disabled LiteInst host-hybrid cells selected from ten clean
/// first-attempt strict repetitions each at the recorded historical source.
/// The current shared Detcore robust-exit change is disclosed in the dated
/// qualification report; this is not a fresh measurement of that runtime.
pub const LITEINST_2026_09_17_EVIDENCE_SHA: &str = "26bda94103ad20cf1d572bac5bc50217826eebed";
pub const LITEINST_2026_09_17_EVIDENCE_DETCORE_TREE: &str =
    "779213274890cd37daafae32ea5a73c1f84090a6";
pub const LITEINST_2026_09_17_EVIDENCE_COMPLETED_UTC: &str = "2026-09-17T00:18:06Z";
pub const LITEINST_2026_09_17_SELECTED_CI_CELL_COUNT: usize =
    LITEINST_2026_09_17_TIMEOUT_CALIBRATIONS.len();
/// Among the 182 KVM ratchet cells covered by the ordinary defaults, retained
/// passing evidence has one to three samples per cell. These are the largest
/// bounds produced by the owner-approved formula.
pub const KVM_RATCHET_DEFAULT_COVERED_MAX_REQUIRED_CPU_SECONDS: u64 = 5;
pub const KVM_RATCHET_DEFAULT_COVERED_MAX_REQUIRED_WALL_SECONDS: u64 = 22;
/// Among the 487 calibrated cells without an explicit override, these are the
/// largest bounds produced by the owner-approved formula.
pub const DEFAULT_COVERED_MAX_REQUIRED_CPU_SECONDS: u64 = 12;
pub const DEFAULT_COVERED_MAX_REQUIRED_WALL_SECONDS: u64 = 49;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeoutCalibration {
    pub test: &'static str,
    pub mode: &'static str,
    pub backend: &'static str,
    pub samples: usize,
    pub p90_cpu_usec: u64,
    pub p90_wall_millis: u64,
    pub required_cpu_seconds: u64,
    pub required_wall_seconds: u64,
    pub configured_cpu_seconds: u64,
    pub configured_wall_seconds: u64,
}

/// The only selected cells whose retained p90-derived bounds exceed at least
/// one ordinary default at [`TIMEOUT_CALIBRATION_CUTOFF_UTC`].
pub const EXPLICIT_TIMEOUT_CALIBRATIONS: [TimeoutCalibration; 5] = [
    TimeoutCalibration {
        test: "applications/kvm-python-examples",
        mode: "verify",
        backend: "kvm",
        samples: 58,
        p90_cpu_usec: 16_656_796,
        p90_wall_millis: 18_271,
        required_cpu_seconds: 25,
        required_wall_seconds: 74,
        configured_cpu_seconds: 25,
        configured_wall_seconds: 74,
    },
    TimeoutCalibration {
        test: "applications/timed-progress-bar",
        mode: "verify",
        backend: "ptrace",
        samples: 60,
        p90_cpu_usec: 20_710_660,
        p90_wall_millis: 22_531,
        required_cpu_seconds: 32,
        required_wall_seconds: 91,
        configured_cpu_seconds: 32,
        configured_wall_seconds: 91,
    },
    TimeoutCalibration {
        test: "c-programs/fp-reduction-nondeterminism",
        mode: "chaos",
        backend: "ptrace",
        samples: 59,
        p90_cpu_usec: 30_251_855,
        p90_wall_millis: 34_964,
        required_cpu_seconds: 46,
        required_wall_seconds: 105,
        configured_cpu_seconds: 46,
        configured_wall_seconds: 105,
    },
    TimeoutCalibration {
        test: "data-handling/dd-partial-transfers",
        mode: "verify",
        backend: "ptrace",
        samples: 60,
        p90_cpu_usec: 12_732_855,
        p90_wall_millis: 14_275,
        required_cpu_seconds: 20,
        required_wall_seconds: 58,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 58,
    },
    TimeoutCalibration {
        test: "data-handling/zstd-multithread",
        mode: "verify",
        backend: "ptrace",
        samples: 59,
        p90_cpu_usec: 37_109_853,
        p90_wall_millis: 39_112,
        required_cpu_seconds: 56,
        required_wall_seconds: 118,
        configured_cpu_seconds: 56,
        configured_wall_seconds: 118,
    },
];

/// KVM cells whose qualification evidence needs a bound above the ordinary
/// default. This is separate from the earlier full retained-data census so its
/// source revision and sample count stay explicit.
pub const KVM_RATCHET_TIMEOUT_CALIBRATIONS: [TimeoutCalibration; 1] = [TimeoutCalibration {
    test: "applications/timed-progress-bar",
    mode: "verify",
    backend: "kvm",
    samples: 3,
    p90_cpu_usec: 12_809_181,
    p90_wall_millis: 13_753,
    required_cpu_seconds: 20,
    required_wall_seconds: 56,
    configured_cpu_seconds: 24,
    configured_wall_seconds: 80,
}];

/// Per-cell nearest-rank p90 from ten schema-4 result rows, using row-level
/// CPU microseconds and wall milliseconds, including reported preparation.
/// See docs/LITEINST_QUALIFICATION_20260916.md for retained evidence and limits.
pub const LITEINST_2026_09_16_TIMEOUT_CALIBRATIONS: [TimeoutCalibration; 22] = [
    TimeoutCalibration {
        test: "backend-parity-c/aio-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_662_030,
        p90_wall_millis: 3_013,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/cwd-roundtrip",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_782_568,
        p90_wall_millis: 3_048,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/event-delivery-ordering",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_757_211,
        p90_wall_millis: 3_174,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/eventfd-semantics",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_810_155,
        p90_wall_millis: 3_137,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fcntl-owner",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_720_047,
        p90_wall_millis: 3_170,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/file-io-roundtrip",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_773_049,
        p90_wall_millis: 3_126,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/membarrier-query",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_759_121,
        p90_wall_millis: 3_083,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/mkdir-rmdir",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_726_751,
        p90_wall_millis: 3_025,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/o-tmpfile-anon",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_836_947,
        p90_wall_millis: 3_233,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/personality-domain",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_692_162,
        p90_wall_millis: 3_048,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/pipe-capacity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_800_792,
        p90_wall_millis: 3_190,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/record-lock",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_788_952,
        p90_wall_millis: 3_126,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/sendfile-copy",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_683_977,
        p90_wall_millis: 3_031,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/set-tid-address",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_647_848,
        p90_wall_millis: 2_982,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/signal-delivery-sequence",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 2_136_272,
        p90_wall_millis: 3_621,
        required_cpu_seconds: 4,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/symlink-ops",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_752_774,
        p90_wall_millis: 3_212,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/umask-mode",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 2_053_977,
        p90_wall_millis: 3_551,
        required_cpu_seconds: 4,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/vectored-io",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_680_463,
        p90_wall_millis: 3_065,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/ioctl-fioclex",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_924_698,
        p90_wall_millis: 3_265,
        required_cpu_seconds: 3,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/pause-alarm-interrupt",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 2_014_495,
        p90_wall_millis: 3_393,
        required_cpu_seconds: 4,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "system-utils/clock-determinism",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_809_704,
        p90_wall_millis: 3_229,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "system-utils/record-getpid",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_714_785,
        p90_wall_millis: 3_090,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
];

/// Per-cell nearest-rank p90 from ten original schema-4 result rows, including
/// preparation in row-level wall time. All raw sample hashes are retained in
/// the separately bound calibration evidence; ordinary 22/57 bounds stay fixed.
/// See docs/LITEINST_QUALIFICATION_20260917.md for source and runtime limits.
pub const LITEINST_2026_09_17_TIMEOUT_CALIBRATIONS: [TimeoutCalibration; 96] = [
    TimeoutCalibration {
        test: "backend-parity-c/append-pwrite",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_832_898,
        p90_wall_millis: 3_580,
        required_cpu_seconds: 3,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/bind-getsockname",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_688_604,
        p90_wall_millis: 3_108,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/cachestat-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_769_174,
        p90_wall_millis: 3_105,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/child-subreaper-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_680_221,
        p90_wall_millis: 3_026,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/close-range-fds",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_689_260,
        p90_wall_millis: 3_062,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/copy-file-range-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_744_373,
        p90_wall_millis: 3_130,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/cpu-virtualization",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_811_671,
        p90_wall_millis: 3_129,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/dup-shared-offset",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_728_572,
        p90_wall_millis: 3_149,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/epoll-pwait2",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_677_649,
        p90_wall_millis: 3_049,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/epoll-readiness",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_650_912,
        p90_wall_millis: 3_008,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/faccessat2-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_693_160,
        p90_wall_millis: 3_014,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fadvise-hints",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_779_288,
        p90_wall_millis: 3_213,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fallocate-extents",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_794_049,
        p90_wall_millis: 3_238,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fchmod-bits",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_818_502,
        p90_wall_millis: 3_567,
        required_cpu_seconds: 3,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fchmodat2-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_614_199,
        p90_wall_millis: 2_943,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fd-duplication",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_677_753,
        p90_wall_millis: 3_131,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/file-backed-mmap",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_738_160,
        p90_wall_millis: 3_118,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/flock-lifecycle",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_773_037,
        p90_wall_millis: 3_127,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/fsync-durability",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_881_428,
        p90_wall_millis: 3_191,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/ftruncate-sparse",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_682_296,
        p90_wall_millis: 3_105,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/getcpu-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_851_621,
        p90_wall_millis: 3_219,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/getpriority-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_603_569,
        p90_wall_millis: 2_991,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/hardware-trap-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_906_337,
        p90_wall_millis: 5_682,
        required_cpu_seconds: 3,
        required_wall_seconds: 23,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/host-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_594_430,
        p90_wall_millis: 2_846,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/inline-syscall-sites",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_624_046,
        p90_wall_millis: 2_909,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/inotify-watch",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_859_000,
        p90_wall_millis: 3_110,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/ioctl-fionread",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_851_234,
        p90_wall_millis: 3_220,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/kcmp-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_681_324,
        p90_wall_millis: 3_074,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/linkat-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_770_012,
        p90_wall_millis: 3_107,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/lseek-positioning",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_697_584,
        p90_wall_millis: 3_016,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/mce-kill-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_773_989,
        p90_wall_millis: 3_103,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/memfd-create",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_774_282,
        p90_wall_millis: 3_210,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/mempolicy-default",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_700_178,
        p90_wall_millis: 3_183,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/mincore-residency",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_698_075,
        p90_wall_millis: 3_033,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/mixed-inline-and-libc-syscalls",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_763_313,
        p90_wall_millis: 3_096,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/mknod-special",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_951_363,
        p90_wall_millis: 3_257,
        required_cpu_seconds: 3,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/msync-writeback",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_729_079,
        p90_wall_millis: 3_147,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/name-to-handle-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_929_776,
        p90_wall_millis: 3_258,
        required_cpu_seconds: 3,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/no-new-privs-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_695_564,
        p90_wall_millis: 3_055,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/numa-node-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_871_997,
        p90_wall_millis: 3_227,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/openat-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_665_762,
        p90_wall_millis: 3_164,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/openat2-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_786_933,
        p90_wall_millis: 3_109,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/path-file-ops",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_711_247,
        p90_wall_millis: 3_111,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/pidfd-open-self",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_752_879,
        p90_wall_millis: 3_185,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/pipe-capacity-pin",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_866_925,
        p90_wall_millis: 3_277,
        required_cpu_seconds: 3,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/pipe-ipc",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_668_560,
        p90_wall_millis: 3_023,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/pipe2-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_746_333,
        p90_wall_millis: 3_111,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/poll-readiness",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_709_153,
        p90_wall_millis: 3_047,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/prctl-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_709_577,
        p90_wall_millis: 3_032,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/prctl-pdeathsig",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_883_598,
        p90_wall_millis: 3_148,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/preadv2-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_824_312,
        p90_wall_millis: 3_161,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/pthread-lifecycle",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_780_677,
        p90_wall_millis: 3_541,
        required_cpu_seconds: 3,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/readdir-entries",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_740_023,
        p90_wall_millis: 3_503,
        required_cpu_seconds: 3,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/readdir-order-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 9_500_190,
        p90_wall_millis: 12_415,
        required_cpu_seconds: 15,
        required_wall_seconds: 50,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/rename-ops",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_707_139,
        p90_wall_millis: 3_114,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/renameat2-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_666_763,
        p90_wall_millis: 3_012,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/rlimit-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_749_417,
        p90_wall_millis: 3_042,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/robust-list",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_796_965,
        p90_wall_millis: 3_101,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/sched-getaffinity-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_781_854,
        p90_wall_millis: 3_595,
        required_cpu_seconds: 3,
        required_wall_seconds: 15,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/seccomp-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_622_968,
        p90_wall_millis: 2_926,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/short-io-split-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_703_127,
        p90_wall_millis: 3_021,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/shutdown-socketpair",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_602_921,
        p90_wall_millis: 2_990,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/signal-waitstatus-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_729_651,
        p90_wall_millis: 5_832,
        required_cpu_seconds: 3,
        required_wall_seconds: 24,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/signalfd-create",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_707_936,
        p90_wall_millis: 3_155,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/socket-epoll-ordering",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_870_409,
        p90_wall_millis: 3_764,
        required_cpu_seconds: 3,
        required_wall_seconds: 16,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/socket-options",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_648_150,
        p90_wall_millis: 3_057,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/socketpair-flags",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_690_636,
        p90_wall_millis: 2_995,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/sockname-unnamed",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_771_363,
        p90_wall_millis: 3_203,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/statfs-free-determinism",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_630_318,
        p90_wall_millis: 3_027,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/statx-metadata",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_602_812,
        p90_wall_millis: 2_950,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/sync-file-range",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_670_801,
        p90_wall_millis: 3_046,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/sysv-ipc-refusal",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_758_770,
        p90_wall_millis: 3_042,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/thp-disable",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_591_929,
        p90_wall_millis: 2_912,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/uname-identity",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_665_613,
        p90_wall_millis: 3_070,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/utimensat-determinism",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_753_246,
        p90_wall_millis: 3_118,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/vectored-file-io",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_748_805,
        p90_wall_millis: 3_043,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/acct-refusal-probe",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_863_522,
        p90_wall_millis: 3_158,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/add-key-enosys",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_736_037,
        p90_wall_millis: 3_033,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/adjtimex-deterministic",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_666_804,
        p90_wall_millis: 3_106,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/bpf-enosys",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_802_251,
        p90_wall_millis: 3_360,
        required_cpu_seconds: 3,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/cachestat-enosys",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_762_500,
        p90_wall_millis: 3_060,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/clock-adjtime-deterministic",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_711_479,
        p90_wall_millis: 3_083,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/clone",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_673_058,
        p90_wall_millis: 3_021,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/copy-file-range-refusal-probe",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_718_281,
        p90_wall_millis: 3_037,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/dbt-copied-tiocgpgrp",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_752_948,
        p90_wall_millis: 3_427,
        required_cpu_seconds: 3,
        required_wall_seconds: 14,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/dbt-exec-failure",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_677_976,
        p90_wall_millis: 2_999,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/dbt-mmap-exec",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_678_470,
        p90_wall_millis: 3_172,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/dbt-prlimit-self",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_684_683,
        p90_wall_millis: 2_999,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/dbt-self-sigqueue",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_799_892,
        p90_wall_millis: 3_173,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/dbt-wait-lifecycle",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 2_060_224,
        p90_wall_millis: 3_936,
        required_cpu_seconds: 4,
        required_wall_seconds: 16,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/fp-reduction-nondeterminism",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 2_423_709,
        p90_wall_millis: 3_946,
        required_cpu_seconds: 4,
        required_wall_seconds: 16,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/futex-requeue-enosys",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_754_734,
        p90_wall_millis: 3_233,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/futex-waitv-enosys",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_648_814,
        p90_wall_millis: 3_011,
        required_cpu_seconds: 3,
        required_wall_seconds: 13,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/futex-wake-enosys",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_661_738,
        p90_wall_millis: 2_960,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "c-programs/get-robust-list-child",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_593_175,
        p90_wall_millis: 2_902,
        required_cpu_seconds: 3,
        required_wall_seconds: 12,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
    TimeoutCalibration {
        test: "backend-parity-c/cpuid-probe",
        mode: "verify",
        backend: "liteinst",
        samples: 10,
        p90_cpu_usec: 1_772_262,
        p90_wall_millis: 5_897,
        required_cpu_seconds: 3,
        required_wall_seconds: 24,
        configured_cpu_seconds: 22,
        configured_wall_seconds: 57,
    },
];

const fn ceil_ratio(numerator: u64, denominator: u64) -> u64 {
    let quotient = numerator / denominator;
    if numerator.is_multiple_of(denominator) {
        quotient
    } else {
        quotient + 1
    }
}

pub const fn cpu_bound_from_p90_usec(p90_cpu_usec: u64) -> u64 {
    ceil_ratio(p90_cpu_usec.saturating_mul(3), 2_000_000)
}

pub const fn wall_bound_from_p90_millis(p90_wall_millis: u64) -> u64 {
    let four_x = p90_wall_millis.saturating_mul(4);
    let scaled = if four_x > 120_000 {
        p90_wall_millis.saturating_mul(3)
    } else {
        four_x
    };
    ceil_ratio(scaled, 1_000)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimeoutMultipliers {
    pub cpu: f64,
    pub wall: f64,
}

impl Default for TimeoutMultipliers {
    fn default() -> Self {
        Self {
            cpu: 1.0,
            wall: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedTestTimeouts {
    pub cpu_seconds: u64,
    pub wall_seconds: u64,
}

pub fn validate_timeout_multiplier(value: f64, name: &str) -> Result<f64, String> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(format!(
            "{name} must be finite and greater than zero, got {value}"
        ))
    }
}

pub fn parse_timeout_multiplier(value: Option<&str>, name: &str) -> Result<f64, String> {
    let Some(value) = value else {
        return Ok(1.0);
    };
    let parsed = value.parse::<f64>().map_err(|error| {
        format!("{name} must be a positive finite number, got {value:?}: {error}")
    })?;
    validate_timeout_multiplier(parsed, name)
}

pub fn timeout_multiplier_from_env(name: &str) -> Result<f64, String> {
    match std::env::var(name) {
        Ok(value) => parse_timeout_multiplier(Some(&value), name),
        Err(std::env::VarError::NotPresent) => parse_timeout_multiplier(None, name),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} must be valid UTF-8")),
    }
}

pub fn timeout_multipliers_from_env() -> Result<TimeoutMultipliers, String> {
    Ok(TimeoutMultipliers {
        cpu: timeout_multiplier_from_env(TEST_CPU_TIMEOUT_MULTIPLIER_ENV)?,
        wall: timeout_multiplier_from_env(TEST_WALL_TIMEOUT_MULTIPLIER_ENV)?,
    })
}

/// Scale a positive whole-second bound conservatively: any fractional result rounds upward.
pub fn scale_timeout_seconds(base: u64, multiplier: f64, name: &str) -> Result<u64, String> {
    validate_timeout_seconds(base, name)?;
    validate_timeout_multiplier(multiplier, name)?;
    let scaled = (base as f64 * multiplier).ceil();
    if scaled > u64::MAX as f64 {
        return Err(format!(
            "{name} overflows whole seconds after applying x{multiplier}"
        ));
    }
    Ok((scaled as u64).max(1))
}

pub fn resolve_test_timeouts(
    cpu_base_seconds: u64,
    wall_base_seconds: u64,
    multipliers: TimeoutMultipliers,
) -> Result<ResolvedTestTimeouts, String> {
    let cpu_seconds = scale_timeout_seconds(cpu_base_seconds, multipliers.cpu, "CPU timeout")?;
    let wall_seconds = scale_timeout_seconds(wall_base_seconds, multipliers.wall, "wall timeout")?;
    if wall_seconds <= cpu_seconds {
        return Err(format!(
            "scaled wall timeout must remain greater than scaled CPU timeout, got wall={wall_seconds}s cpu={cpu_seconds}s"
        ));
    }
    Ok(ResolvedTestTimeouts {
        cpu_seconds,
        wall_seconds,
    })
}

#[allow(dead_code)]
pub fn validate_timeout_seconds(value: u64, context: &str) -> Result<u64, String> {
    if (MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "{context}: timeout_seconds must be {MIN_TIMEOUT_SECONDS}..={MAX_TIMEOUT_SECONDS}"
        ))
    }
}

pub fn resolve_timeout_seconds(
    global_default: u64,
    bucket_override: Option<u64>,
    cell_override: Option<u64>,
) -> u64 {
    cell_override.or(bucket_override).unwrap_or(global_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_resolution_is_global_then_bucket_then_cell() {
        assert_eq!(resolve_timeout_seconds(15, None, None), 15);
        assert_eq!(resolve_timeout_seconds(15, Some(20), None), 20);
        assert_eq!(resolve_timeout_seconds(15, Some(20), Some(30)), 30);
    }

    #[test]
    fn timeout_bounds_are_closed() {
        assert_eq!(validate_timeout_seconds(1, "fixture").unwrap(), 1);
        assert_eq!(validate_timeout_seconds(1800, "fixture").unwrap(), 1800);
        assert!(validate_timeout_seconds(0, "fixture").is_err());
        assert!(validate_timeout_seconds(1801, "fixture").is_err());
    }

    #[test]
    fn owner_defaults_are_the_conservative_ceilings() {
        assert_eq!(DEFAULT_TEST_CPU_TIMEOUT_SECONDS, 22);
        assert_eq!(DEFAULT_TEST_WALL_TIMEOUT_SECONDS, 57);
        assert_eq!(cpu_bound_from_p90_usec(14_298_000), 22);
        assert_eq!(wall_bound_from_p90_millis(14_019), 57);
    }

    #[test]
    fn retained_p90_calibrations_recompute_and_cover_every_exception() {
        fn assert_covers(configured: u64, required: u64) {
            assert!(configured >= required);
        }

        assert_eq!(TIMEOUT_CALIBRATION_CUTOFF_UTC, "2026-09-03T02:18:30Z");
        assert_eq!(
            CALIBRATED_CI_CELL_COUNT,
            DEFAULT_COVERED_CI_CELL_COUNT + EXPLICIT_TIMEOUT_CALIBRATIONS.len()
        );
        assert_eq!(
            KVM_RATCHET_CALIBRATION_SHA,
            "92bacf12deba6a717f77cfcbd6afefc5ffb383f2"
        );
        assert_eq!(
            KVM_RATCHET_CALIBRATION_COMPLETED_UTC,
            "2026-09-04T04:39:00Z"
        );
        assert_eq!(
            KVM_TIMED_PROGRESS_BAR_REQUALIFICATION_SHA,
            "f190205a7b3e65e7ebf347ba0875b668fd72d9a7"
        );
        assert_eq!(
            KVM_TIMED_PROGRESS_BAR_REQUALIFICATION_COMPLETED_UTC,
            "2026-09-05T03:43:05Z"
        );
        assert_eq!(
            KVM_RATCHET_CI_CELL_COUNT,
            KVM_RATCHET_DEFAULT_COVERED_CI_CELL_COUNT + KVM_RATCHET_TIMEOUT_CALIBRATIONS.len()
        );
        assert_covers(
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
            DEFAULT_COVERED_MAX_REQUIRED_CPU_SECONDS,
        );
        assert_covers(
            DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
            DEFAULT_COVERED_MAX_REQUIRED_WALL_SECONDS,
        );
        assert_covers(
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
            KVM_RATCHET_DEFAULT_COVERED_MAX_REQUIRED_CPU_SECONDS,
        );
        assert_covers(
            DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
            KVM_RATCHET_DEFAULT_COVERED_MAX_REQUIRED_WALL_SECONDS,
        );
        assert_eq!(
            KVM_NEXT40_QUALIFICATION_SHA,
            "4d8f866102882b6eeabdf99cc7e81433cc3c95c5"
        );
        assert_eq!(
            KVM_NEXT40_QUALIFICATION_COMPLETED_UTC,
            "2026-09-04T20:26:39Z"
        );
        assert_eq!(KVM_NEXT40_QUALIFIED_CI_CELL_COUNT, 30);
        assert_covers(
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
            KVM_NEXT40_DEFAULT_COVERED_MAX_REQUIRED_CPU_SECONDS,
        );
        assert_covers(
            DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
            KVM_NEXT40_DEFAULT_COVERED_MAX_REQUIRED_WALL_SECONDS,
        );
        assert_eq!(
            KVM_2026_09_08_EVIDENCE_SHA,
            "9d4bb692ddfe02241e6341a2faeb83782215e1a5"
        );
        assert_eq!(
            KVM_2026_09_08_EVIDENCE_COMPLETED_UTC,
            "2026-09-08T14:03:16Z"
        );
        assert_eq!(KVM_2026_09_08_SELECTED_CI_CELL_COUNT, 21);
        assert_covers(
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
            KVM_2026_09_08_MAX_REQUIRED_CPU_SECONDS,
        );
        assert_covers(
            DEFAULT_TEST_WALL_TIMEOUT_SECONDS,
            KVM_2026_09_08_MAX_REQUIRED_WALL_SECONDS,
        );
        assert_eq!(
            IPC_DETERMINISM_CHAOS_EVIDENCE_SHA,
            "0b26fb782192e017ef9103e27d017f8c73ceeeb4"
        );
        assert_eq!(
            IPC_DETERMINISM_CHAOS_EVIDENCE_COMPLETED_UTC,
            "2026-09-15T22:07:28Z"
        );
        assert_eq!(IPC_DETERMINISM_CHAOS_SELECTED_CI_CELL_COUNT, 1);
        assert_eq!(NON_CI_CELL_COUNT, 177);
        for calibration in EXPLICIT_TIMEOUT_CALIBRATIONS
            .iter()
            .chain(&KVM_RATCHET_TIMEOUT_CALIBRATIONS)
            .chain(&LITEINST_2026_09_16_TIMEOUT_CALIBRATIONS)
            .chain(&LITEINST_2026_09_17_TIMEOUT_CALIBRATIONS)
            .copied()
        {
            assert!(calibration.samples > 0);
            assert_eq!(
                calibration.required_cpu_seconds,
                cpu_bound_from_p90_usec(calibration.p90_cpu_usec),
                "{} {}/{} CPU formula drifted",
                calibration.test,
                calibration.mode,
                calibration.backend
            );
            assert_eq!(
                calibration.required_wall_seconds,
                wall_bound_from_p90_millis(calibration.p90_wall_millis),
                "{} {}/{} wall formula drifted",
                calibration.test,
                calibration.mode,
                calibration.backend
            );
            assert!(calibration.configured_cpu_seconds >= calibration.required_cpu_seconds);
            assert!(calibration.configured_wall_seconds >= calibration.required_wall_seconds);
        }
    }

    #[test]
    fn machine_scaling_is_independent_and_rounds_up() {
        assert_eq!(scale_timeout_seconds(22, 1.01, "CPU").unwrap(), 23);
        assert_eq!(scale_timeout_seconds(57, 1.01, "wall").unwrap(), 58);
        let policy = resolve_test_timeouts(
            22,
            57,
            TimeoutMultipliers {
                cpu: 1.5,
                wall: 2.0,
            },
        )
        .unwrap();
        assert_eq!(policy.cpu_seconds, 33);
        assert_eq!(policy.wall_seconds, 114);
    }

    #[test]
    fn invalid_or_inverted_scaled_policy_is_refused() {
        for multiplier in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(scale_timeout_seconds(57, multiplier, "wall").is_err());
        }
        assert!(
            resolve_test_timeouts(
                22,
                57,
                TimeoutMultipliers {
                    cpu: 3.0,
                    wall: 1.0,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn multiplier_values_default_independently_and_refuse_bad_input() {
        assert_eq!(parse_timeout_multiplier(None, "CPU").unwrap(), 1.0);
        assert_eq!(parse_timeout_multiplier(Some("1.25"), "CPU").unwrap(), 1.25);
        assert_eq!(parse_timeout_multiplier(Some("2"), "wall").unwrap(), 2.0);
        for value in ["", "nope", "0", "-1", "NaN", "inf"] {
            assert!(
                parse_timeout_multiplier(Some(value), "fixture multiplier").is_err(),
                "accepted {value:?}"
            );
        }
    }
}
