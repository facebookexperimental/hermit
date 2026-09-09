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
pub const NON_CI_CELL_COUNT: usize = 178;
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
        assert_eq!(NON_CI_CELL_COUNT, 178);
        for calibration in EXPLICIT_TIMEOUT_CALIBRATIONS
            .iter()
            .chain(&KVM_RATCHET_TIMEOUT_CALIBRATIONS)
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
