/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use detcore_model::summary::PathEvidence;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use sha2::Digest;
use sha2::Sha256;

use crate::backend_parity::BackendParityReport;
use crate::canonical_verdict::InfrastructureError;
use crate::canonical_verdict::Verdict;
pub use crate::canonical_verdict::VerificationReport;
pub use crate::canonical_verdict::VerificationRuntime;
use crate::ci_selection::CiDisabledReasonSpec;
use crate::ci_selection::CiSelection;
use crate::ci_selection::CiSelectionSpec;
use crate::cpu_evidence::CellCpuBinding;
use crate::cpu_evidence::CellCpuObservationsV1;
use crate::cpu_evidence::ChargeBasis;
use crate::cpu_evidence::CommandIdentity;
use crate::cpu_evidence::CpuError;
use crate::cpu_evidence::CpuErrorStage;
use crate::cpu_evidence::FinalCpuSource;
use crate::cpu_evidence::FinalWaitObservation;
use crate::cpu_evidence::InvocationCpuObservation;
use crate::cpu_evidence::InvocationRole;
use crate::cpu_evidence::LaunchObservation;
use crate::cpu_evidence::LiveCpuObservation;
use crate::cpu_evidence::NotStartedReason;
use crate::cpu_evidence::RegistrationObservation;
use crate::cpu_evidence::ReturnedCpuCharge;
use crate::cpu_evidence::SpawnStage;
use crate::cpu_evidence::TerminationPath;
use crate::cpu_evidence::WaitCpuObservation;
use crate::cpu_evidence::WaitOperation;
use crate::environmental_block::EnvBlockClass;
use crate::environmental_block::environmental_block_observation;
use crate::host_capability::probe_host_capabilities;
use crate::ledger::RequiredNullable;
use crate::stress_series::HostCapabilities;
use crate::stress_series::HostCapability;
#[cfg(test)]
use crate::stress_series::HostCapabilityVerdict;
#[cfg(test)]
use crate::timeouts::CALIBRATED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::DBT_MATRIX_2026_09_29_ENABLED_CI_FALSE_TESTS;
#[cfg(test)]
use crate::timeouts::DBT_MATRIX_2026_09_29_PROMOTED_CI_FALSE_TESTS;
#[cfg(test)]
use crate::timeouts::DBT_MATRIX_2026_09_29_SELECTED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::DEFAULT_TEST_CPU_TIMEOUT_SECONDS;
#[cfg(test)]
use crate::timeouts::DEFAULT_TEST_WALL_TIMEOUT_SECONDS;
use crate::timeouts::DEFAULTS_FILE;
#[cfg(test)]
use crate::timeouts::EXPLICIT_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::IPC_DETERMINISM_CHAOS_SELECTED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_2026_09_08_SELECTED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_NEXT40_QUALIFIED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_RATCHET_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::KVM_RATCHET_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::KVM_RUN_1709_CI_REMOVAL_COUNT;
#[cfg(test)]
use crate::timeouts::LITEINST_2026_09_16_SELECTED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::LITEINST_2026_09_16_TIMEOUT_CALIBRATIONS;
#[cfg(test)]
use crate::timeouts::LITEINST_2026_09_17_SELECTED_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::LITEINST_2026_09_17_TIMEOUT_CALIBRATIONS;
use crate::timeouts::MANIFEST_SCHEMA;
#[cfg(test)]
use crate::timeouts::NON_CI_CELL_COUNT;
#[cfg(test)]
use crate::timeouts::PTRACE_2026_09_24_SELECTED_CI_CELL_COUNT;
use crate::timeouts::ResolvedTestTimeouts;
#[cfg(test)]
use crate::timeouts::STRICT_COMPAT_FOLD_2026_10_01_SELECTED_CI_CELL_COUNT;
use crate::timeouts::TimeoutMultipliers;
use crate::timeouts::resolve_test_timeouts;
use crate::timeouts::resolve_timeout_seconds;
use crate::timeouts::timeout_multipliers_from_env;
use crate::timeouts::validate_timeout_seconds;

const BACKENDS: [&str; 5] = ["ptrace", "dbt", "kvm", "sabre", "liteinst"];
const MODES: [&str; 5] = ["verify", "chaos", "replay", "naked", "custom"];
const CELL_CPU_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CELL_CPU_ACCOUNTING_GRACE: Duration = Duration::from_secs(1);
pub const CELL_RESULT_SCHEMA: u64 = 4;
pub const E2E_MACHINE_SHORTNAME_ENV: &str = "E2E_MACHINE_SHORTNAME";
pub const E2E_KERNEL_VERSION_ENV: &str = "E2E_KERNEL_VERSION";
pub const E2E_RUN_INDEX_ENV: &str = "E2E_RUN_INDEX";

/// The commit the cells' source came from and whether the tree is dirty: the
/// named `source_sha` of a Git-less snapshot (clean by construction), else
/// what Git reports for the checkout at `root`.
fn source_identity(root: &Path, source_sha: Option<&str>) -> Result<(String, bool), String> {
    match source_sha {
        Some(sha) => {
            validate_source_sha(sha)?;
            Ok((sha.to_string(), false))
        }
        None => Ok((
            git(root, &["rev-parse", "HEAD"])?,
            !git(root, &["status", "--porcelain", "--untracked-files=no"])?.is_empty(),
        )),
    }
}

/// A `--source-sha` must name one commit exactly: 40 lowercase hex digits, the
/// spelling `git rev-parse HEAD` records in `hermit_sha`.
pub fn validate_source_sha(sha: &str) -> Result<(), String> {
    if sha.len() == 40
        && sha
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(format!(
            "--source-sha must be a full 40-digit lowercase commit sha, got {sha:?}; \
             pass the output of `git rev-parse HEAD` for the archived commit"
        ))
    }
}

fn first_attempt() -> u64 {
    1
}

/// Closed vocabulary for manifest `requires` tokens.
///
/// The optional value is the host capability whose proven absence may withhold
/// a cell. A token mapped to `None` is still validated, but can never suppress
/// execution. Keeping the mapping here gives the manifest validator and the
/// executable harness one authority.
pub const REQUIRES_VOCABULARY: &[(&str, Option<HostCapability>)] = &[
    ("ar", None),
    ("bash", None),
    ("cc", None),
    ("cpuid", Some(HostCapability::CpuidFaulting)),
    ("cxx", None),
    ("date", None),
    ("du", None),
    ("find", None),
    ("gawk", None),
    ("git", None),
    ("hexdump", None),
    ("jq", None),
    ("kvm", None),
    ("linux", None),
    ("lua5.4", None),
    ("m4", None),
    ("node", None),
    ("openssl", None),
    ("perl", None),
    ("ptrace", None),
    ("python3", None),
    ("ruby", None),
    ("rustc", None),
    ("sqlite3", None),
    ("tclsh", None),
    ("userns", None),
    // The host kernel exports `__vdso_getrandom` (Linux 6.11+ on x86-64). A
    // golden that prints the vDSO getrandom leg is only reachable on such a
    // kernel; see [`VDSO_GETRANDOM_GOLDEN_MARKER`]. Descriptive only, like
    // `cc`: nothing probes it, so on an older kernel the cell runs and fails
    // with the declared prerequisite named, instead of being withheld.
    ("vdso-getrandom", None),
    ("x86_64", None),
    ("zstd", None),
];

/// The text tests/c/random_sources.c prints only when the host kernel exports
/// `__vdso_getrandom`. On an older kernel it prints `vdso-getrandom
/// unavailable` instead, so an exact golden carrying this marker has a kernel
/// floor, and the test must declare it with the `vdso-getrandom` requires token.
pub const VDSO_GETRANDOM_GOLDEN_MARKER: &str = "vdso-getrandom[";

/// Refuse a verify golden that silently assumes a Linux 6.11+ host.
///
/// The manifest must say which host a golden was measured on when the golden
/// cannot be produced elsewhere; otherwise an older-kernel FAIL reads as a
/// determinism regression.
pub fn validate_golden_kernel_floor(
    id: &str,
    requires: &[String],
    modes: &BTreeMap<String, ModeRecipe>,
) -> Result<(), String> {
    if requires.iter().any(|token| token == "vdso-getrandom") {
        return Ok(());
    }
    for (mode, recipe) in modes {
        for (key, table) in [
            ("expected_stdout", &recipe.expected_stdout),
            ("expected_stdout_contains", &recipe.expected_stdout_contains),
        ] {
            if let Some(backend) = table.iter().find_map(|(backend, text)| {
                text.contains(VDSO_GETRANDOM_GOLDEN_MARKER)
                    .then_some(backend)
            }) {
                return Err(format!(
                    "{id}: {mode} {key}.{backend} prints the vDSO getrandom leg, which only a \
                     Linux 6.11+ host exports; declare `vdso-getrandom` in requires"
                ));
            }
        }
    }
    Ok(())
}

/// The host capability a backend itself needs, independent of any `requires`
/// token: a kvm cell cannot run where KVM is proven absent.
pub fn backend_capability(backend: &str) -> Option<HostCapability> {
    (backend == "kvm").then_some(HostCapability::Kvm)
}

pub fn requires_capability(token: &str) -> Result<Option<HostCapability>, String> {
    REQUIRES_VOCABULARY
        .iter()
        .find(|(name, _)| *name == token)
        .map(|(_, capability)| *capability)
        .ok_or_else(|| {
            format!(
                "unknown `requires` token `{token}`; the vocabulary is closed and an unrecognized \
                 name is refused rather than treated as a reason to omit a cell"
            )
        })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDefaults {
    pub schema: u64,
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
    #[serde(default)]
    pub nextest: Vec<NextestTimeout>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NextestTimeout {
    pub filter: String,
    pub timeout_seconds: u64,
    pub slow_reason: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestDocument {
    pub schema: u64,
    pub bucket: String,
    pub timeout_seconds: Option<u64>,
    pub slow_reason: Option<String>,
    pub test: Vec<TestRecipe>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestRecipe {
    pub id: String,
    pub description: String,
    pub lane: String,
    #[serde(default)]
    pub requires: Vec<String>,
    pub occasional: bool,
    pub program: Option<String>,
    pub direct: Option<DirectCommand>,
    pub observation: Observation,
    pub build: Option<BuildRecipe>,
    pub modes: BTreeMap<String, ModeRecipe>,
    #[serde(default)]
    pub preprocessors: Vec<String>,
    /// The run types this test belongs to, such as `strict-compat`. A bucket
    /// node selects with `test-harness run --label LABEL` the tests carrying
    /// any of its labels, so one bucket serves several run types and each run
    /// type keeps only its own tests.
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DirectCommand {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRecipe {
    #[serde(default)]
    pub cflags: Vec<String>,
    #[serde(default)]
    pub rustflags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub status: bool,
    pub stdout: bool,
    pub stderr: bool,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeRecipe {
    pub ci: CiSelectionSpec,
    pub ci_disabled_reason: Option<CiDisabledReasonSpec>,
    #[serde(default)]
    pub backends_enabled: Vec<String>,
    #[serde(default)]
    pub backends_disabled: BTreeMap<String, String>,
    #[serde(default)]
    pub guest_args: BTreeMap<String, Vec<String>>,
    pub workdir: Option<String>,
    pub runs: Option<u64>,
    pub seeds: Option<Vec<i64>>,
    pub assert: Option<Assertions>,
    pub outcome_classes: Option<u64>,
    #[serde(default)]
    pub args: Vec<String>,
    pub compare_io_buffers: Option<bool>,
    pub compare_io_buffers_disabled_reason: Option<String>,
    pub rcb_time: Option<bool>,
    pub rcb_time_disabled_reason: Option<String>,
    #[serde(default)]
    pub timeout_seconds: BTreeMap<String, u64>,
    #[serde(default)]
    pub cpu_timeout_seconds: BTreeMap<String, u64>,
    #[serde(default)]
    pub slow_reason: BTreeMap<String, String>,
    pub expected_guest_exit: Option<ExpectedGuestExit>,
    /// The exact guest stdout a verify cell requires, per enabled backend.
    ///
    /// A matched verify comparison proves the two runs agree with each other;
    /// it cannot notice that both printed the same wrong bytes. Naming the
    /// bytes here makes the cell also require both compared runs' stdout to
    /// equal them, so declaring the same bytes for two backends of one test
    /// makes those backends agree with each other as well.
    #[serde(default)]
    pub expected_stdout: BTreeMap<String, String>,
    /// Text a verify cell's guest stdout must contain, per enabled backend.
    ///
    /// For output that is repeatable within one host but not byte-stable
    /// across hosts (addresses, timing deltas), the exact bytes cannot be
    /// declared. The cell can still require the success marker the guest
    /// prints, so two runs that agree on output lacking it do not pass.
    #[serde(default)]
    pub expected_stdout_contains: BTreeMap<String, String>,
    /// Hermit `run` flags a verify cell adds after the runner's own, for a test
    /// whose recorded policy runs a specific configuration. Only the flags in
    /// ALLOWED_HERMIT_ARGS are accepted, each relaxes determinism, so
    /// `hermit_args_reason` is required and both are recorded as relaxations.
    #[serde(default)]
    pub hermit_args: Vec<String>,
    pub hermit_args_reason: Option<String>,
    /// Guest environment variables a verify cell adds, as `--env NAME=VALUE`,
    /// after the runner's fixed guest environment. A name the runner sets is
    /// refused rather than silently overridden.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The comparison a verify cell's two runs must pass. `strict` (the
    /// default) is the canonical full-observation comparison that can
    /// establish L2. `stripped` is Hermit's default `--verify` comparison:
    /// below L2, kept only for corpora whose recorded verdict policy is that
    /// comparison, never counted as bitwise parity, and a relaxation that
    /// requires `comparator_reason` and is recorded.
    pub comparator: Option<Comparator>,
    pub comparator_reason: Option<String>,
    /// Backends on which this verify cell is a DIAGNOSTIC, each with the
    /// standing reason: a product failure of the cell is reported and counted
    /// as a diagnostic failure, recorded as a relaxation, and does not fail
    /// the run. Every other outcome keeps its meaning, so an ERROR (the cell
    /// could not produce a product verdict) still fails it. Only a
    /// `comparator: stripped` cell may declare one: a canonical cell exists to
    /// supply L2 evidence, and its divergence must keep failing.
    #[serde(default)]
    pub diagnostic: BTreeMap<String, String>,
    /// Why a product failure of this verify cell is final. Every other cell
    /// is retried once after a product failure; this one gets one attempt,
    /// for a corpus whose recorded verdict was a single run (the strict
    /// compatibility corpus), so a first-attempt failure stays a failure.
    pub no_retry_reason: Option<String>,
}

/// The Hermit the harness runs: `HERMIT_BIN`, else target/debug/hermit. A
/// relative path names a checkout path, like the default, because cells start
/// Hermit from their own working directories; an absolute path is kept.
pub fn resolve_hermit_bin(root: &Path, hermit_bin: Option<std::ffi::OsString>) -> PathBuf {
    root.join(
        hermit_bin
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/debug/hermit")),
    )
}

/// Whether a product failure of `cell` earns the runner's one retry: false
/// for a verify cell that declares `no_retry_reason`.
pub fn retries_product_failures(cell: &SelectedCell) -> bool {
    cell.test
        .modes
        .get(&cell.id.mode)
        .is_none_or(|mode| mode.no_retry_reason.is_none())
}

/// A stripped cell's report must hold the comparison it declared: Hermit's
/// stripped log comparison, with virtual time, over a non-empty event stream
/// on both runs. A report without one is incomplete evidence, never a pass.
/// The scorecard admits a stripped PASS by this same rule.
pub fn require_stripped_comparison(report: &VerificationReport) -> Result<(), String> {
    let counts = report.compared_log_messages.as_ref();
    let Some(comparison) = report.comparison.as_ref() else {
        return Err(format!(
            "stripped verification recorded no comparison at all (verdict={})",
            report.verdict
        ));
    };
    if comparison.strictness == crate::canonical_verdict::LogCompareStrictness::Stripped
        && comparison.compare_logs
        && comparison.virtualize_time == Some(true)
        && counts.is_some_and(|counts| counts.left > 0 && counts.right > 0)
    {
        Ok(())
    } else {
        Err(format!(
            "stripped verification did not compare non-vacuous stripped evidence with virtual time: strictness={} compare_logs={} virtualize_time={:?} messages={}/{}",
            comparison.strictness,
            comparison.compare_logs,
            comparison.virtualize_time,
            counts.map_or(0, |counts| counts.left),
            counts.map_or(0, |counts| counts.right),
        ))
    }
}

/// The report the real Hermit wrote for `run --strict --verify
/// --base-env=minimal -- /bin/echo hermit-compat` at hermit d44bbbb79acd
/// (validate profile): its default, stripped comparison. One copy for every
/// test that reads a stripped report (runner, ledger cell verdict, schema 10).
#[cfg(test)]
pub(crate) const PRODUCER_STRIPPED_REPORT: &str = r#"{"verified":true,"bitwise_parity":false,"verdict":"matched","no_result_reason":null,"infrastructure_error":null,"comparison":{"strictness":"stripped","display_name":"Stripped","compare_logs":true,"compare_io_buffers":true,"log_scope":"deterministic","record_envelope":"all_records_v1","virtualize_time":true,"strip_lines":true,"canonicalize_addresses":false,"full_trace":false,"exact_remainder":false,"stripped_prefixes":["real-wall-clock-prefix/v1","unsafe-numeric-address-and-path-normalization/v1"],"canonicalizations":[],"ignore_lines":false,"skip_commit":false,"skip_detlog":false},"compared_log_messages":{"left":243,"right":243},"compared_outputs":{"left":{"exit_code":0,"signal":null,"stdout_sha256":"26d1520b716304e2aab518cbf28242104d67fc9127d28c3de4193abe8aa87ba1","stdout_bytes":14,"stderr_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","stderr_bytes":0},"right":{"exit_code":0,"signal":null,"stdout_sha256":"26d1520b716304e2aab518cbf28242104d67fc9127d28c3de4193abe8aa87ba1","stdout_bytes":14,"stderr_sha256":"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855","stderr_bytes":0}},"runtime":{"run1":{"scheduler_turns":6,"virtual_nanoseconds":4126085,"syscalls":40},"run2":{"scheduler_turns":6,"virtual_nanoseconds":4126085,"syscalls":40}},"guest_exit_code":0,"guest_signal":null,"first_divergent_scheduler_turn":null,"first_divergent_virtual_nanoseconds":null,"first_divergent_record":null,"first_divergent_syscall":null,"first_divergent_left_message":null,"first_divergent_right_message":null}"#;

/// The comparison a verify cell requires; see [`ModeRecipe::comparator`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Comparator {
    Strict,
    Stripped,
}

/// The one nonzero guest disposition a verify cell requires.
///
/// Hermit's default `--verify` policy rejects a first run that does not exit
/// 0, so a guest that fails on purpose never reaches a comparison. Naming the
/// exact status here runs the cell with `--verify-allow=failure` and then
/// requires the report's guest disposition AND Hermit's own process status to
/// match it, so a different code, a different signal, or a success all fail.
///
/// Hermit reports a guest's signal death either by dying from the same signal
/// or, where it cannot re-raise it, by exiting `128 + signo`; both are the
/// same disposition, and the report's `guest_signal` pins which signal it was.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedGuestExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: String,
}

impl ExpectedGuestExit {
    /// Whether Hermit's own process status reports this guest disposition.
    pub fn hermit_status_matches(&self, code: Option<i32>, signal: Option<i32>) -> bool {
        match (self.code, self.signal) {
            (Some(expected), None) => code == Some(expected) && signal.is_none(),
            (None, Some(expected)) => {
                (code.is_none() && signal == Some(expected))
                    || (code == Some(128 + expected) && signal.is_none())
            }
            _ => false,
        }
    }

    fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(code), None) => format!("exit code {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            _ => "an invalid expectation".into(),
        }
    }

    /// Check the completed comparison's guest disposition and Hermit's status.
    fn check(
        &self,
        guest_exit_code: Option<i32>,
        guest_signal: Option<i32>,
        hermit_code: Option<i32>,
        hermit_signal: Option<i32>,
    ) -> Result<(), String> {
        let observed = match (guest_exit_code, guest_signal) {
            (Some(code), None) => format!("exit code {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => "no guest disposition".into(),
            (Some(code), Some(signal)) => format!("exit code {code} and signal {signal}"),
        };
        if guest_exit_code != self.code || guest_signal != self.signal {
            return Err(format!(
                "guest ended with {observed}, but the manifest expects {}",
                self.describe()
            ));
        }
        if !self.hermit_status_matches(hermit_code, hermit_signal) {
            let hermit = match (hermit_code, hermit_signal) {
                (Some(code), _) => format!("exited with status {code}"),
                (None, Some(signal)) => format!("was killed by signal {signal}"),
                (None, None) => "reported no status".into(),
            };
            return Err(format!(
                "guest ended with {observed}, but hermit {hermit}, which does not report {}",
                self.describe()
            ));
        }
        Ok(())
    }
}

/// The declared guest disposition for a selected cell; only verify cells carry one.
fn cell_expected_guest_exit(cell: &SelectedCell) -> Option<ExpectedGuestExit> {
    (cell.id.mode == "verify")
        .then(|| cell.test.modes.get(&cell.id.mode))
        .flatten()
        .and_then(|recipe| recipe.expected_guest_exit.clone())
}

/// The exact stdout a selected verify cell requires, if its backend declares one.
fn cell_expected_stdout(cell: &SelectedCell) -> Option<String> {
    (cell.id.mode == "verify")
        .then(|| cell.test.modes.get(&cell.id.mode))
        .flatten()
        .and_then(|recipe| {
            cell.id
                .backend
                .as_ref()
                .and_then(|backend| recipe.expected_stdout.get(backend).cloned())
        })
}

/// The stdout text a selected verify cell must contain, if its backend declares one.
fn cell_expected_stdout_contains(cell: &SelectedCell) -> Option<String> {
    (cell.id.mode == "verify")
        .then(|| cell.test.modes.get(&cell.id.mode))
        .flatten()
        .and_then(|recipe| {
            cell.id
                .backend
                .as_ref()
                .and_then(|backend| recipe.expected_stdout_contains.get(backend).cloned())
        })
}

/// A per-backend stdout assertion is verify-only and keyed by enabled backends.
///
/// A key for a disabled backend would be an assertion that ordinary runs never
/// execute, so it is refused rather than kept as a silent, unchecked claim.
fn validate_verify_stdout_table(
    id: &str,
    mode: &str,
    key: &str,
    table: &BTreeMap<String, String>,
    enabled: &BTreeSet<&str>,
) -> Result<(), String> {
    if table.is_empty() {
        return Ok(());
    }
    if mode != "verify" {
        return Err(format!("{id}: {key} is supported only by verify mode"));
    }
    for backend in table.keys() {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: verify {key} names backend {backend} outside backends_enabled"
            ));
        }
    }
    Ok(())
}

/// `expected_stdout` is a verify-only assertion keyed by enabled backends.
pub fn validate_expected_stdout(
    id: &str,
    mode: &str,
    expected: &BTreeMap<String, String>,
    enabled: &BTreeSet<&str>,
) -> Result<(), String> {
    validate_verify_stdout_table(id, mode, "expected_stdout", expected, enabled)
}

/// `expected_stdout_contains` is verify-only, keyed by enabled backends, and
/// names non-empty text: every stream contains the empty string, so an empty
/// declaration would read as an assertion while checking nothing.
pub fn validate_expected_stdout_contains(
    id: &str,
    mode: &str,
    contains: &BTreeMap<String, String>,
    enabled: &BTreeSet<&str>,
) -> Result<(), String> {
    validate_verify_stdout_table(id, mode, "expected_stdout_contains", contains, enabled)?;
    if let Some(backend) = contains
        .iter()
        .find_map(|(backend, text)| text.is_empty().then_some(backend))
    {
        return Err(format!(
            "{id}: verify expected_stdout_contains.{backend} must not be empty"
        ));
    }
    Ok(())
}

/// Why a verify report does not satisfy a declared `expected_stdout`.
#[derive(Debug, Eq, PartialEq)]
enum ExpectedStdoutError {
    /// The report has no per-run outputs, so the assertion cannot be decided.
    Unevidenced(String),
    /// A compared run printed different bytes.
    Mismatch(String),
}

/// The reason a verify attempt FAILs when the `run` ("first" or "second")
/// compared run's stdout is not the cell's declared `expected_stdout`.
///
/// This text is the only record of that failure: the attempt carries no
/// timeout and no error kind. The scorecard reader
/// (ci/compat-envelope/scorecard.rs) and the series `failed_match` evidence
/// recognize it through [`parse_expected_output_failure_reason`], which
/// rebuilds it with this function, so the writer and the readers share one
/// spelling.
pub fn expected_stdout_mismatch_reason(
    run: &str,
    observed_bytes: u64,
    observed_sha256: &str,
    expected_bytes: u64,
    expected_sha256: &str,
) -> String {
    format!(
        "verify {run} run stdout ({observed_bytes} bytes, sha256 {observed_sha256}) differs from the declared expected_stdout ({expected_bytes} bytes, sha256 {expected_sha256})"
    )
}

/// The reason a verify attempt FAILs when its captured stdout, which equals
/// both compared runs' stdout, omits the cell's declared
/// `expected_stdout_contains` text. Shared with the scorecard reader exactly
/// as [`expected_stdout_mismatch_reason`] is.
pub fn expected_stdout_contains_mismatch_reason(
    captured_bytes: u64,
    captured_sha256: &str,
    text: &str,
) -> String {
    format!(
        "verify stdout ({captured_bytes} bytes, sha256 {captured_sha256}) does not contain the declared expected_stdout_contains text {text:?}"
    )
}

/// What a reason written by [`expected_stdout_mismatch_reason`] or
/// [`expected_stdout_contains_mismatch_reason`] says. The scorecard reader and
/// the series `failed_match` evidence both recognize those reasons through
/// [`parse_expected_output_failure_reason`], so the runner, the scorecard and
/// the series schema share one spelling and one parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExpectedOutputFailureReason {
    /// The named compared run's stdout is not the declared exact bytes.
    Exact {
        run: String,
        observed_bytes: u64,
        observed_sha256: String,
        declared_bytes: u64,
        declared_sha256: String,
    },
    /// The captured stdout, which equals both compared runs' stdout, omits
    /// the declared text.
    Contains {
        captured_bytes: u64,
        captured_sha256: String,
        text: String,
    },
}

impl ExpectedOutputFailureReason {
    /// Why the runner could not have written this reason for a verify
    /// attempt whose two compared runs matched, judged from the reason alone,
    /// or `None`. The runner names the first run whose stdout differs, and
    /// both runs of a matched pair print the same bytes; it fails only
    /// declared stdout that differs from the observed stdout; and it finds an
    /// empty declared text in any stdout.
    pub fn contradiction(&self) -> Option<&'static str> {
        match self {
            Self::Exact {
                run,
                observed_bytes,
                observed_sha256,
                declared_bytes,
                declared_sha256,
            } => {
                if !is_lower_sha256(observed_sha256) || !is_lower_sha256(declared_sha256) {
                    Some("a stdout digest is not SHA-256")
                } else if run != "first" {
                    Some(
                        "it names a later run, but the first run of a matched pair printed the same bytes",
                    )
                } else if observed_bytes == declared_bytes && observed_sha256 == declared_sha256 {
                    Some("the declared stdout is the observed stdout")
                } else {
                    None
                }
            }
            Self::Contains {
                captured_sha256,
                text,
                ..
            } => {
                if !is_lower_sha256(captured_sha256) {
                    Some("the stdout digest is not SHA-256")
                } else if text.is_empty() {
                    Some("the runner finds an empty declared text in any stdout")
                } else {
                    None
                }
            }
        }
    }
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Split `<bytes> bytes, sha256 <digest>)<rest>`.
fn split_sized_digest(text: &str) -> Option<(u64, &str, &str)> {
    let (bytes, rest) = text.split_once(" bytes, sha256 ")?;
    let (digest, rest) = rest.split_once(')')?;
    Some((bytes.parse().ok()?, digest, rest))
}

/// Invert Rust's `{:?}` rendering of a string, which is how the runner quotes
/// a declared expected_stdout_contains text. The caller still requires the
/// re-rendered reason to reproduce the original exactly, so a lenient decode
/// here cannot admit a text the runner would not write.
fn parse_debug_string(quoted: &str) -> Option<String> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut text = String::new();
    let mut chars = inner.chars();
    while let Some(character) = chars.next() {
        match character {
            '"' => return None,
            '\\' => text.push(match chars.next()? {
                't' => '\t',
                'r' => '\r',
                'n' => '\n',
                '0' => '\0',
                '\\' => '\\',
                '"' => '"',
                '\'' => '\'',
                'u' => {
                    let (hex, rest) = chars.as_str().strip_prefix('{')?.split_once('}')?;
                    let decoded = char::from_u32(u32::from_str_radix(hex, 16).ok()?)?;
                    chars = rest.chars();
                    decoded
                }
                _ => return None,
            }),
            other => text.push(other),
        }
    }
    Some(text)
}

/// Recognize the runner's reason for a failed stdout expectation, or `None`
/// for any other text. Only a reason that the runner's own formatter
/// rebuilds byte for byte is recognized.
pub fn parse_expected_output_failure_reason(reason: &str) -> Option<ExpectedOutputFailureReason> {
    if let Some(rest) = reason.strip_prefix("verify stdout (") {
        let (captured_bytes, captured_sha256, rest) = split_sized_digest(rest)?;
        let quoted =
            rest.strip_prefix(" does not contain the declared expected_stdout_contains text ")?;
        let text = parse_debug_string(quoted)?;
        (expected_stdout_contains_mismatch_reason(captured_bytes, captured_sha256, &text) == reason)
            .then(|| ExpectedOutputFailureReason::Contains {
                captured_bytes,
                captured_sha256: captured_sha256.into(),
                text,
            })
    } else {
        let (run, rest) = reason
            .strip_prefix("verify ")?
            .split_once(" run stdout (")?;
        let (observed_bytes, observed_sha256, rest) = split_sized_digest(rest)?;
        let rest = rest.strip_prefix(" differs from the declared expected_stdout (")?;
        let (declared_bytes, declared_sha256, rest) = split_sized_digest(rest)?;
        (rest.is_empty()
            && expected_stdout_mismatch_reason(
                run,
                observed_bytes,
                observed_sha256,
                declared_bytes,
                declared_sha256,
            ) == reason)
            .then(|| ExpectedOutputFailureReason::Exact {
                run: run.into(),
                observed_bytes,
                observed_sha256: observed_sha256.into(),
                declared_bytes,
                declared_sha256: declared_sha256.into(),
            })
    }
}

/// Compare both runs a verify report compared with the declared stdout bytes.
///
/// The report carries each run's stdout digest and length, so the check needs
/// no captured stream: a run whose stdout differs in any byte fails. A report
/// without per-run outputs cannot satisfy the assertion and is never a pass.
fn check_expected_stdout(
    expected: &str,
    report: &VerificationReport,
) -> Result<(), ExpectedStdoutError> {
    let outputs = report.compared_outputs.as_ref().ok_or_else(|| {
        ExpectedStdoutError::Unevidenced(
            "verification report omitted compared_outputs, so the declared expected_stdout cannot be checked".into(),
        )
    })?;
    let expected_sha256 = hex_digest(expected.as_bytes());
    let expected_bytes = expected.len() as u64;
    for (run, output) in [("first", &outputs.left), ("second", &outputs.right)] {
        if output.stdout_sha256 != expected_sha256 || output.stdout_bytes != expected_bytes {
            return Err(ExpectedStdoutError::Mismatch(
                expected_stdout_mismatch_reason(
                    run,
                    output.stdout_bytes,
                    &output.stdout_sha256,
                    expected_bytes,
                    &expected_sha256,
                ),
            ));
        }
    }
    Ok(())
}

/// Require the captured verify stdout to contain the declared text.
///
/// The report describes both compared runs' stdout only by digest and length,
/// so the captured stream is evidence for both runs only when it equals both of
/// them; only then is the text searched for. A capture that is not those bytes,
/// or a report without per-run outputs, decides nothing and is never a pass.
fn check_expected_stdout_contains(
    text: &str,
    captured: &[u8],
    report: &VerificationReport,
) -> Result<(), ExpectedStdoutError> {
    let outputs = report.compared_outputs.as_ref().ok_or_else(|| {
        ExpectedStdoutError::Unevidenced(
            "verification report omitted compared_outputs, so the declared expected_stdout_contains cannot be checked".into(),
        )
    })?;
    let captured_sha256 = hex_digest(captured);
    let captured_bytes = captured.len() as u64;
    for (run, output) in [("first", &outputs.left), ("second", &outputs.right)] {
        if output.stdout_sha256 != captured_sha256 || output.stdout_bytes != captured_bytes {
            return Err(ExpectedStdoutError::Unevidenced(format!(
                "captured verify stdout ({captured_bytes} bytes, sha256 {captured_sha256}) is not the {run} compared run's stdout ({} bytes, sha256 {}), so the declared expected_stdout_contains cannot be checked",
                output.stdout_bytes, output.stdout_sha256
            )));
        }
    }
    let needle = text.as_bytes();
    if needle.is_empty()
        || captured
            .windows(needle.len())
            .any(|window| window == needle)
    {
        Ok(())
    } else {
        Err(ExpectedStdoutError::Mismatch(
            expected_stdout_contains_mismatch_reason(captured_bytes, &captured_sha256, text),
        ))
    }
}

pub fn validate_expected_guest_exit(
    id: &str,
    mode: &str,
    expected: Option<&ExpectedGuestExit>,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if mode != "verify" {
        return Err(format!(
            "{id}: expected_guest_exit is supported only by verify mode"
        ));
    }
    match (expected.code, expected.signal) {
        (Some(code), None) if (1..=255).contains(&code) => {}
        (Some(code), None) => {
            return Err(format!(
                "{id}: verify expected_guest_exit.code must be a nonzero exit status in 1..=255, not {code}"
            ));
        }
        (None, Some(signal)) if (1..=64).contains(&signal) => {}
        (None, Some(signal)) => {
            return Err(format!(
                "{id}: verify expected_guest_exit.signal must be a signal number in 1..=64, not {signal}"
            ));
        }
        _ => {
            return Err(format!(
                "{id}: verify expected_guest_exit must name exactly one of code or signal"
            ));
        }
    }
    if expected.reason.trim().is_empty() {
        return Err(format!(
            "{id}: verify expected_guest_exit.reason must be substantive"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assertions {
    pub bitwise_parity: Option<bool>,
    pub min_distinct: Option<u64>,
    pub min_passes: Option<u64>,
    pub min_failures: Option<u64>,
    pub min_normalized_entropy: Option<f64>,
    pub runs: Option<u64>,
    pub repeat_identical: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct ManifestSet {
    pub documents: Vec<ManifestDocument>,
    tests: BTreeMap<String, (String, u64, u64, TestRecipe)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct CellId {
    pub test: String,
    pub mode: String,
    pub backend: Option<String>,
}

const SCHEDULED_JOBS_ENV: &str = "HERMIT_E2E_SCHEDULED_JOBS";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
const FIXED_GUEST_WORKDIR: &str = "/tmp/test";
const FIXED_WORKDIR_SOURCE_DIR: &str = "workdir";

/// One per-cell input directory a verify cell binds to a fixed guest path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EqualizedInput {
    /// The directory below the cell directory (`prepare_dirs` creates it).
    pub cell_subdir: &'static str,
    /// The guest environment variable that names the directory.
    pub env: &'static str,
    /// Where the guest sees the directory: below [`EQUALIZED_INPUT_ROOT`].
    pub guest_path: &'static str,
}

/// Guest directory below which a verify cell's per-cell inputs are bound.
///
/// Every cell runs from its own directory, and that directory's name contains
/// the backend (see [`cell_artifact_path`]). Handing the guest those host
/// paths gives the ptrace and candidate runs of one test a different HOME,
/// XDG_CONFIG_HOME, fixture directory and program path, and those strings
/// alone make ptrace diverge from ptrace: 16 of 18 pairs in the S0 control
/// (<https://github.com/rrnewton/hermit/issues/3301#issuecomment-5874842696>).
/// Binding each directory to the same guest path, and naming only guest paths
/// in the guest's environment and argv, gives every backend that honours
/// `--bind` the same guest-visible inputs. The dbt backend refuses `--bind`,
/// so its cells keep the host paths (see [`equalizes_guest_inputs`]).
///
/// It is below guest `/tmp` because `--bind` accepts only targets there;
/// every such guest gets a private tmpfs `/tmp`, in which the bind targets
/// are created.
pub const EQUALIZED_INPUT_ROOT: &str = "/tmp/e2e";

/// The inputs bound below [`EQUALIZED_INPUT_ROOT`], in argv order.
pub const EQUALIZED_INPUTS: [EqualizedInput; 3] = [
    EqualizedInput {
        cell_subdir: "home",
        env: "HOME",
        guest_path: "/tmp/e2e/home",
    },
    EqualizedInput {
        cell_subdir: "xdg-config",
        env: "XDG_CONFIG_HOME",
        guest_path: "/tmp/e2e/xdg-config",
    },
    EqualizedInput {
        cell_subdir: "fixtures",
        env: "E2E_FIXTURE_DIR",
        guest_path: "/tmp/e2e/fixtures",
    },
];

/// Whether a cell's guest is given the equalized inputs.
///
/// Only `verify` cells are, because only they are compared across backends
/// (the parity post-pass). Other modes keep the host paths they always had.
/// The dbt backend refuses `--bind` (`hermit-cli/src/bin/hermit/run.rs`: "the
/// dbt backend cannot apply --mount or --bind"), so a dbt cell cannot be given
/// them and its parity rows say `inputs-not-equalized`.
pub fn equalizes_guest_inputs(mode: &str, backend: &str) -> bool {
    mode == "verify" && backend != "dbt"
}

/// The guest path of a host path at or below one of `cell_dir`'s equalized
/// input directories, or `None` for any other value.
fn equalized_guest_path(cell_dir: &Path, value: &str) -> Option<String> {
    EQUALIZED_INPUTS.iter().find_map(|input| {
        let host = cell_dir.join(input.cell_subdir);
        let host = host.to_str()?;
        if value == host {
            Some(input.guest_path.to_string())
        } else {
            value
                .strip_prefix(host)
                .filter(|rest| rest.starts_with('/'))
                .map(|rest| format!("{}{rest}", input.guest_path))
        }
    })
}

/// `guest_argv` with each whole argument that names a path in one of the
/// cell's equalized input directories rewritten to its guest path, so the
/// program is launched as `/tmp/e2e/fixtures/program`. A path embedded in a
/// longer argument is left alone; it still names a host file the guest can
/// read, and the parity post-pass then sees unequal argv and reports the
/// comparison as not equalized.
fn equalized_guest_argv(cell_dir: &Path, guest_argv: Vec<String>) -> Vec<String> {
    guest_argv
        .into_iter()
        .map(|arg| equalized_guest_path(cell_dir, &arg).unwrap_or(arg))
        .collect()
}

/// The cell environment as the guest of an equalized cell sees it: each
/// equalized variable names its guest path. The row keeps the host paths,
/// which name where the directories really are.
fn equalized_guest_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = env.clone();
    for input in EQUALIZED_INPUTS {
        env.insert(input.env.into(), input.guest_path.into());
    }
    env
}

/// Bind each equalized input directory of `cell_dir` to its guest path.
fn append_equalized_input_binds(argv: &mut Vec<String>, cell_dir: &Path) {
    for input in EQUALIZED_INPUTS {
        argv.push(format!(
            "--bind={}:{}",
            cell_dir.join(input.cell_subdir).to_string_lossy(),
            input.guest_path
        ));
    }
}

fn fixed_workdir_source_for_attempt(cell_dir: &Path, attempt: &str) -> Result<PathBuf, String> {
    let mut components = Path::new(attempt).components();
    let is_one_normal_component = matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == OsStr::new(attempt)
    );
    if !is_one_normal_component {
        return Err(format!(
            "invalid attempt label {attempt:?}: expected exactly one normal path component"
        ));
    }
    Ok(cell_dir.join(FIXED_WORKDIR_SOURCE_DIR).join(attempt))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Population {
    Required,
    Enabled,
    Disabled,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub population: Option<Population>,
    pub lane: Option<String>,
    pub category: Option<String>,
    pub test: Option<String>,
    pub mode: Option<String>,
    pub backend: Option<String>,
    /// Hermit backends whose cells this selection omits.
    ///
    /// The only committed caller is the GitHub-hosted portable profile, whose
    /// runners expose `/dev/kvm` without the PMU counter the KVM guest clock
    /// needs. Those cells stay in the local `full` and `portable` profiles; the
    /// hosted profile's expected population omits them by the same list, so an
    /// omitted cell is never counted as a pass.
    pub exclude_backends: Vec<String>,
    /// Manifest categories (buckets) whose cells this selection omits. The
    /// committed caller is the quick profile's pinned-root verify smoke, which
    /// omits the host-bound compatibility corpus by
    /// [`crate::validation_dag::QUICK_EXCLUDED_CATEGORIES`].
    pub exclude_categories: Vec<String>,
    pub include_occasional: bool,
    pub include_manual: bool,
    /// When non-empty, only tests carrying at least one of these labels.
    pub labels: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SelectedCell {
    pub category: String,
    pub test: TestRecipe,
    pub id: CellId,
    pub enabled: bool,
    pub timeout_seconds: u64,
    pub cpu_timeout_seconds: u64,
}

/// Named manifest bytes captured once for shared parsing and provenance.
/// Keeping these inputs together prevents a consumer from hashing one revision
/// while independently parsing another revision at the same paths.
pub(crate) struct ManifestInputs {
    sources: Vec<(PathBuf, String)>,
}

impl ManifestInputs {
    pub(crate) fn read(root: &Path) -> Result<Self, String> {
        let dir = root.join("tests/e2e/manifests");
        let defaults_path = dir.join(DEFAULTS_FILE);
        let defaults_source = fs::read_to_string(&defaults_path)
            .map_err(|e| format!("cannot read {}: {e}", defaults_path.display()))?;
        let mut paths = fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|e| format!("cannot read an entry in {}: {e}", dir.display()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "yaml")
                    && path.file_name().is_some_and(|name| name != DEFAULTS_FILE)
            })
            .collect::<Vec<_>>();
        paths.sort();
        if paths.is_empty() {
            return Err(format!("no YAML manifests found in {}", dir.display()));
        }
        let mut sources = vec![(defaults_path, defaults_source)];
        for path in paths {
            let source = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            sources.push((path, source));
        }
        sources.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(Self { sources })
    }

    pub(crate) fn named_sources(&self) -> impl Iterator<Item = (&Path, &str)> {
        self.sources
            .iter()
            .map(|(path, source)| (path.as_path(), source.as_str()))
    }
}

impl ManifestSet {
    pub fn load(root: &Path) -> Result<Self, String> {
        Self::from_inputs(root, &ManifestInputs::read(root)?)
    }

    pub(crate) fn from_inputs(root: &Path, inputs: &ManifestInputs) -> Result<Self, String> {
        let (defaults_path, defaults_source) = inputs
            .named_sources()
            .find(|(path, _)| path.file_name().is_some_and(|name| name == DEFAULTS_FILE))
            .ok_or_else(|| "captured manifest inputs lack defaults.yaml".to_string())?;
        let defaults: ManifestDefaults = serde_yaml::from_str(defaults_source)
            .map_err(|e| format!("{}: invalid YAML: {e}", defaults_path.display()))?;
        if defaults.schema != MANIFEST_SCHEMA {
            return Err(format!(
                "{}: schema must be {MANIFEST_SCHEMA}",
                defaults_path.display()
            ));
        }
        let global_timeout_seconds =
            validate_timeout_seconds(defaults.timeout_seconds, "global default")?;
        let global_cpu_timeout_seconds =
            validate_timeout_seconds(defaults.cpu_timeout_seconds, "global CPU default")?;
        let mut nextest_filters = BTreeSet::new();
        for timeout in &defaults.nextest {
            if timeout.filter.trim().is_empty() {
                return Err("nextest timeout filter must not be empty".into());
            }
            validate_timeout_seconds(timeout.timeout_seconds, &timeout.filter)?;
            if timeout.timeout_seconds == global_timeout_seconds {
                return Err(format!(
                    "nextest timeout {} redundantly repeats the global default",
                    timeout.filter
                ));
            }
            if timeout.slow_reason.trim().is_empty() {
                return Err(format!(
                    "nextest timeout {} requires a non-empty slow_reason",
                    timeout.filter
                ));
            }
            if !nextest_filters.insert(timeout.filter.as_str()) {
                return Err(format!(
                    "duplicate nextest timeout filter: {}",
                    timeout.filter
                ));
            }
        }
        let mut documents = Vec::new();
        let mut tests = BTreeMap::new();
        for (path, source) in inputs
            .named_sources()
            .filter(|(path, _)| path.file_name().is_some_and(|name| name != DEFAULTS_FILE))
        {
            let document: ManifestDocument = serde_yaml::from_str(source)
                .map_err(|e| format!("{}: invalid YAML: {e}", path.display()))
                .and_then(|value| {
                    crate::manifest_corpus::expand_corpus(value)
                        .map_err(|e| format!("{}: {e}", path.display()))
                })
                .and_then(|value| {
                    serde_yaml::from_value(value)
                        .map_err(|e| format!("{}: invalid manifest: {e}", path.display()))
                })?;
            let stem = path.file_stem().and_then(OsStr::to_str).unwrap_or_default();
            validate_document_with_cpu(
                &document,
                stem,
                root,
                global_timeout_seconds,
                global_cpu_timeout_seconds,
            )?;
            let bucket_timeout_seconds =
                resolve_timeout_seconds(global_timeout_seconds, document.timeout_seconds, None);
            for test in &document.test {
                if tests
                    .insert(
                        test.id.clone(),
                        (
                            document.bucket.clone(),
                            bucket_timeout_seconds,
                            global_cpu_timeout_seconds,
                            test.clone(),
                        ),
                    )
                    .is_some()
                {
                    return Err(format!("duplicate test id: {}", test.id));
                }
            }
            documents.push(document);
        }
        Ok(Self { documents, tests })
    }

    /// Is `id` a test this manifest set has ever heard of?
    ///
    /// ⚠️ THIS EXISTS TO SEPARATE "NO CELLS" FROM "NO SUCH TEST". `select` returns an
    /// empty vector for both, and they mean opposite things: an unfiltered population
    /// that is legitimately empty ("no gaps") versus a filter naming something that
    /// does not exist (a typo). A caller that cannot tell them apart must either
    /// refuse a good answer or accept a meaningless one.
    pub fn knows_test(&self, id: &str) -> bool {
        self.tests.contains_key(id)
    }

    pub fn select(&self, selection: &Selection) -> Result<Vec<SelectedCell>, String> {
        // A label no test carries is a typo, not an empty run type: with
        // --allow-empty it would otherwise select nothing and pass.
        if let Some(label) = selection.labels.iter().find(|label| {
            !self
                .tests
                .values()
                .any(|(_, _, _, test)| test.labels.contains(label))
        }) {
            return Err(format!("--label {label} names no test in any manifest"));
        }
        // An excluded category no manifest has is a typo that would exclude nothing.
        if let Some(category) = selection.exclude_categories.iter().find(|category| {
            !self
                .tests
                .values()
                .any(|(bucket, _, _, _)| bucket == *category)
        }) {
            return Err(format!(
                "--exclude-category {category} names no manifest category"
            ));
        }
        let population = selection.population.unwrap_or(Population::Enabled);
        let mut cells = Vec::new();
        for (id, (category, bucket_timeout_seconds, default_cpu_timeout_seconds, test)) in
            &self.tests
        {
            if selection
                .lane
                .as_deref()
                .is_some_and(|lane| lane != test.lane)
                || selection
                    .category
                    .as_deref()
                    .is_some_and(|value| value != category)
                || selection.test.as_deref().is_some_and(|value| value != id)
                || selection.exclude_categories.contains(category)
                || (!selection.include_occasional && test.occasional)
                || (!selection.labels.is_empty()
                    && !test
                        .labels
                        .iter()
                        .any(|label| selection.labels.contains(label)))
            {
                continue;
            }
            for (mode, recipe) in &test.modes {
                if selection.mode.as_deref().is_some_and(|value| value != mode) {
                    continue;
                }
                if mode == "naked" {
                    let enabled = recipe
                        .backends_enabled
                        .iter()
                        .any(|value| value == "native");
                    let ci = ci_selection(recipe)?.selected("native");
                    let accepted = match population {
                        Population::Required => {
                            enabled
                                && (ci
                                    || selection.mode.as_deref() == Some("naked")
                                    || selection.include_manual)
                        }
                        other => population_accepts(other, ci, enabled),
                    };
                    if selection
                        .backend
                        .as_deref()
                        .is_some_and(|backend| backend != "native")
                        || !accepted
                    {
                        continue;
                    }
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: None,
                        },
                        enabled,
                        timeout_seconds: resolve_timeout_seconds(
                            *bucket_timeout_seconds,
                            None,
                            recipe.timeout_seconds.get("native").copied(),
                        ),
                        cpu_timeout_seconds: recipe
                            .cpu_timeout_seconds
                            .get("native")
                            .copied()
                            .unwrap_or(*default_cpu_timeout_seconds),
                    });
                    continue;
                }
                let backends: Vec<_> = match population {
                    Population::Disabled => recipe.backends_disabled.keys().cloned().collect(),
                    _ => recipe.backends_enabled.clone(),
                };
                let ci = ci_selection(recipe)?;
                for backend in backends {
                    if selection
                        .backend
                        .as_deref()
                        .is_some_and(|value| value != backend)
                        || selection.exclude_backends.contains(&backend)
                    {
                        continue;
                    }
                    let enabled = recipe.backends_enabled.contains(&backend);
                    if population == Population::Required
                        && !selection.include_manual
                        && !ci.selected(&backend)
                    {
                        continue;
                    }
                    if !population_accepts(population, ci.selected(&backend), enabled) {
                        continue;
                    }
                    let timeout_seconds = resolve_timeout_seconds(
                        *bucket_timeout_seconds,
                        None,
                        recipe.timeout_seconds.get(&backend).copied(),
                    );
                    let cpu_timeout_seconds = recipe
                        .cpu_timeout_seconds
                        .get(&backend)
                        .copied()
                        .unwrap_or(*default_cpu_timeout_seconds);
                    cells.push(SelectedCell {
                        category: category.clone(),
                        test: test.clone(),
                        id: CellId {
                            test: id.clone(),
                            mode: mode.clone(),
                            backend: Some(backend),
                        },
                        enabled,
                        timeout_seconds,
                        cpu_timeout_seconds,
                    });
                }
            }
        }
        cells.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(cells)
    }

    pub fn all_tests(&self) -> impl Iterator<Item = (&str, u64, u64, &TestRecipe)> {
        self.tests
            .values()
            .map(|(category, timeout_seconds, cpu_timeout_seconds, test)| {
                (
                    category.as_str(),
                    *timeout_seconds,
                    *cpu_timeout_seconds,
                    test,
                )
            })
    }
}

fn population_accepts(population: Population, ci: bool, enabled: bool) -> bool {
    match population {
        Population::Required => ci && enabled,
        Population::Enabled => enabled,
        Population::Disabled => !enabled,
    }
}

#[cfg(test)]
fn validate_document(
    document: &ManifestDocument,
    stem: &str,
    root: &Path,
    global_timeout_seconds: u64,
) -> Result<(), String> {
    validate_document_with_cpu(
        document,
        stem,
        root,
        global_timeout_seconds,
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
    )
}

fn validate_document_with_cpu(
    document: &ManifestDocument,
    stem: &str,
    root: &Path,
    global_timeout_seconds: u64,
    global_cpu_timeout_seconds: u64,
) -> Result<(), String> {
    if document.schema != MANIFEST_SCHEMA {
        return Err(format!("{stem}: schema must be {MANIFEST_SCHEMA}"));
    }
    if document.bucket != stem {
        return Err(format!(
            "{stem}: bucket `{}` must equal file stem",
            document.bucket
        ));
    }
    if document.test.is_empty() {
        return Err(format!("{stem}: test list must not be empty"));
    }
    match (document.timeout_seconds, document.slow_reason.as_deref()) {
        (Some(timeout), Some(reason)) if !reason.trim().is_empty() => {
            validate_timeout_seconds(timeout, &format!("{stem} bucket"))?;
            if timeout == global_timeout_seconds {
                return Err(format!(
                    "{stem}: bucket timeout_seconds redundantly repeats the global default"
                ));
            }
        }
        (Some(_), _) => {
            return Err(format!(
                "{stem}: bucket timeout_seconds requires a non-empty slow_reason"
            ));
        }
        (None, Some(_)) => {
            return Err(format!("{stem}: bucket slow_reason has no timeout_seconds"));
        }
        (None, None) => {}
    }
    let bucket_timeout_seconds =
        resolve_timeout_seconds(global_timeout_seconds, document.timeout_seconds, None);
    for test in &document.test {
        if !test.id.starts_with(&format!("{stem}/")) {
            return Err(format!("{}: id must start with {stem}/", test.id));
        }
        if !matches!(test.lane.as_str(), "portable" | "privileged") {
            return Err(format!("{}: invalid lane `{}`", test.id, test.lane));
        }
        for token in &test.requires {
            requires_capability(token).map_err(|error| format!("{}.requires: {error}", test.id))?;
        }
        validate_golden_kernel_floor(&test.id, &test.requires, &test.modes)?;
        match (&test.program, &test.direct) {
            (Some(program), None) => {
                if !program.starts_with("tests/") || !root.join(program).exists() {
                    return Err(format!("{}: missing program {program}", test.id));
                }
            }
            (None, Some(DirectCommand::Shell(command))) if !command.trim().is_empty() => {}
            (None, Some(DirectCommand::Argv(argv))) if !argv.is_empty() => {
                crate::manifest_corpus::check_direct_placeholders(&test.id, argv)?;
            }
            (Some(_), Some(_)) => return Err(format!("{}: set only program or direct", test.id)),
            _ => return Err(format!("{}: missing executable program/direct", test.id)),
        }
        let actual: BTreeSet<_> = test.modes.keys().map(String::as_str).collect();
        let expected: BTreeSet<_> = MODES.into_iter().collect();
        if actual != expected {
            return Err(format!("{}: modes must be exactly {expected:?}", test.id));
        }
        validate_labels(&test.id, &test.labels)?;
        for (mode, recipe) in &test.modes {
            validate_mode_with_cpu(
                &test.id,
                mode,
                recipe,
                bucket_timeout_seconds,
                global_cpu_timeout_seconds,
            )?;
            validate_mode_extensions(&test.id, mode, recipe)?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn validate_mode(
    id: &str,
    mode: &str,
    recipe: &ModeRecipe,
    bucket_timeout_seconds: u64,
) -> Result<(), String> {
    validate_mode_with_cpu(
        id,
        mode,
        recipe,
        bucket_timeout_seconds,
        DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
    )
}

fn validate_mode_with_cpu(
    id: &str,
    mode: &str,
    recipe: &ModeRecipe,
    bucket_timeout_seconds: u64,
    default_cpu_timeout_seconds: u64,
) -> Result<(), String> {
    validate_mode_workdir(
        id,
        mode,
        recipe.workdir.as_deref(),
        &recipe.backends_enabled,
    )?;
    let expected: BTreeSet<&str> = if mode == "naked" {
        ["native"].into_iter().collect()
    } else {
        BACKENDS.into_iter().collect()
    };
    let enabled: BTreeSet<_> = recipe.backends_enabled.iter().map(String::as_str).collect();
    let disabled: BTreeSet<_> = recipe
        .backends_disabled
        .keys()
        .map(String::as_str)
        .collect();
    if enabled.len() != recipe.backends_enabled.len()
        || !enabled.is_disjoint(&disabled)
        || enabled.union(&disabled).copied().collect::<BTreeSet<_>>() != expected
    {
        return Err(format!("{id}: {mode} does not partition {expected:?}"));
    }
    if recipe
        .backends_disabled
        .values()
        .any(|reason| reason.trim().is_empty())
    {
        return Err(format!("{id}: {mode} has an empty backend-disabled reason"));
    }
    let wall_timeout_keys: BTreeSet<_> =
        recipe.timeout_seconds.keys().map(String::as_str).collect();
    let cpu_timeout_keys: BTreeSet<_> = recipe
        .cpu_timeout_seconds
        .keys()
        .map(String::as_str)
        .collect();
    let reason_keys: BTreeSet<_> = recipe.slow_reason.keys().map(String::as_str).collect();
    if wall_timeout_keys != cpu_timeout_keys || wall_timeout_keys != reason_keys {
        return Err(format!(
            "{id}: {mode} timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        ));
    }
    for (backend, timeout) in &recipe.timeout_seconds {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} timeout_seconds names disabled backend {backend}"
            ));
        }
        validate_timeout_seconds(*timeout, &format!("{id}: {mode}/{backend}"))?;
        let reason = recipe.slow_reason.get(backend).ok_or_else(|| {
            format!("{id}: {mode}/{backend} timeout_seconds requires slow_reason")
        })?;
        if reason.trim().is_empty() {
            return Err(format!(
                "{id}: {mode}/{backend} slow_reason must be non-empty"
            ));
        }
    }
    for (backend, timeout) in &recipe.cpu_timeout_seconds {
        if !enabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} cpu_timeout_seconds names disabled backend {backend}"
            ));
        }
        validate_timeout_seconds(*timeout, &format!("{id}: {mode}/{backend} CPU"))?;
        let reason = recipe.slow_reason.get(backend).ok_or_else(|| {
            format!("{id}: {mode}/{backend} cpu_timeout_seconds requires slow_reason")
        })?;
        if reason.trim().is_empty() {
            return Err(format!(
                "{id}: {mode}/{backend} slow_reason must be non-empty"
            ));
        }
    }
    for backend in &wall_timeout_keys {
        if recipe.timeout_seconds[*backend] == bucket_timeout_seconds
            && recipe.cpu_timeout_seconds[*backend] == default_cpu_timeout_seconds
        {
            return Err(format!(
                "{id}: {mode}/{backend} timeout pair redundantly repeats its inherited values"
            ));
        }
    }
    let ci = CiSelection::validate(
        &enabled
            .iter()
            .map(|backend| (*backend).to_string())
            .collect(),
        &disabled
            .iter()
            .map(|backend| (*backend).to_string())
            .collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
    .map_err(|error| format!("{id}: {mode} {error}"))?;
    if mode == "naked" && ci.any_selected() {
        return Err(format!(
            "{id}: naked is opt-in meta-CI and must set ci=false"
        ));
    }
    for (backend, args) in &recipe.guest_args {
        if !enabled.contains(backend.as_str()) && !disabled.contains(backend.as_str()) {
            return Err(format!(
                "{id}: {mode} guest_args names backend {backend} outside backends_enabled/backends_disabled"
            ));
        }
        if args.iter().any(|argument| argument.contains('\0')) {
            return Err(format!(
                "{id}: {mode} guest_args.{backend} contains a NUL byte, which Linux argv cannot represent"
            ));
        }
    }
    match (
        mode,
        recipe.compare_io_buffers,
        recipe.compare_io_buffers_disabled_reason.as_deref(),
    ) {
        ("verify", None | Some(true), None) | ("verify", Some(false), Some(_)) => {}
        ("verify", Some(false), None) => {
            return Err(format!(
                "{id}: verify compare_io_buffers=false requires compare_io_buffers_disabled_reason"
            ));
        }
        ("verify", None | Some(true), Some(_)) => {
            return Err(format!(
                "{id}: verify comparison reason is stale while I/O-buffer comparison is enabled"
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(format!(
                "{id}: compare_io_buffers is supported only by verify mode"
            ));
        }
    }
    if recipe
        .compare_io_buffers_disabled_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!(
            "{id}: compare_io_buffers_disabled_reason must be substantive"
        ));
    }
    match (
        mode,
        recipe.rcb_time,
        recipe.rcb_time_disabled_reason.as_deref(),
    ) {
        ("verify", None | Some(true), None) | ("verify", Some(false), Some(_)) => {}
        ("verify", Some(false), None) => {
            return Err(format!(
                "{id}: verify rcb_time=false requires rcb_time_disabled_reason"
            ));
        }
        ("verify", None | Some(true), Some(_)) => {
            return Err(format!(
                "{id}: verify RCB-time reason is stale while RCB time is enabled"
            ));
        }
        (_, None, None) => {}
        _ => {
            return Err(format!("{id}: rcb_time is supported only by verify mode"));
        }
    }
    if recipe
        .rcb_time_disabled_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!(
            "{id}: rcb_time_disabled_reason must be substantive"
        ));
    }
    validate_expected_guest_exit(id, mode, recipe.expected_guest_exit.as_ref())?;
    validate_expected_stdout(id, mode, &recipe.expected_stdout, &enabled)?;
    validate_expected_stdout_contains(id, mode, &recipe.expected_stdout_contains, &enabled)?;
    Ok(())
}

pub fn validate_mode_workdir(
    id: &str,
    mode: &str,
    workdir: Option<&str>,
    _backends_enabled: &[String],
) -> Result<(), String> {
    let Some(workdir) = workdir else {
        return Ok(());
    };
    if !matches!(mode, "verify" | "chaos" | "custom") {
        return Err(format!(
            "{id}: {mode} workdir is supported only by Hermit run modes"
        ));
    }
    if !Path::new(workdir).is_absolute() {
        return Err(format!("{id}: {mode} workdir must be an absolute path"));
    }
    Ok(())
}

fn supports_test_workdir(mode: &str, _backend: &str) -> bool {
    matches!(mode, "verify" | "replay" | "chaos" | "custom")
}

fn cell_relaxations(cell: &SelectedCell) -> Vec<String> {
    let recipe = &cell.test.modes[&cell.id.mode];
    let mut relaxations = Vec::new();
    if recipe.compare_io_buffers == Some(false) {
        relaxations.push(format!(
            "--no-detlog-io-buffers: {}",
            recipe
                .compare_io_buffers_disabled_reason
                .as_deref()
                .unwrap_or("reason missing")
        ));
    }
    if recipe.rcb_time == Some(false) {
        relaxations.push(format!(
            "--no-rcb-time: {}",
            recipe
                .rcb_time_disabled_reason
                .as_deref()
                .unwrap_or("reason missing")
        ));
    }
    let reason =
        |reason: &Option<String>| reason.as_deref().unwrap_or("reason missing").to_string();
    for arg in &recipe.hermit_args {
        relaxations.push(format!("{arg}: {}", reason(&recipe.hermit_args_reason)));
    }
    if recipe.comparator == Some(Comparator::Stripped) {
        relaxations.push(format!(
            "{STRIPPED_COMPARATOR_RELAXATION_PREFIX}{}",
            reason(&recipe.comparator_reason)
        ));
    }
    if let Some(reason) = cell_diagnostic_reason(cell) {
        relaxations.push(format!("{DIAGNOSTIC_RELAXATION_PREFIX}{reason}"));
    }
    relaxations
}

/// The relaxation that records a stripped-comparator cell and its reason. The
/// ledger reads it back to judge the cell by the comparison it declared.
pub const STRIPPED_COMPARATOR_RELAXATION_PREFIX: &str = "comparator=stripped: ";

/// The relaxation that records a diagnostic cell and its reason. It is the
/// one place a result row says which cell was diagnostic, so
/// [`diagnostic_failure_reason`] reads it back by this exact prefix.
const DIAGNOSTIC_RELAXATION_PREFIX: &str = "diagnostic (a product failure does not fail the run): ";

/// The manifest's diagnostic reason for this cell's backend, if it is one.
fn cell_diagnostic_reason(cell: &SelectedCell) -> Option<&str> {
    let backend = cell.id.backend.as_deref()?;
    cell.test.modes[&cell.id.mode]
        .diagnostic
        .get(backend)
        .map(String::as_str)
}

/// Whether the manifest declares this cell a diagnostic on its backend.
pub fn is_diagnostic_cell(cell: &SelectedCell) -> bool {
    cell.enabled && cell_diagnostic_reason(cell).is_some()
}

/// A result row's classification: `disabled` for a cell outside
/// `backends_enabled`, `diagnostic` for an enabled cell the manifest declares
/// diagnostic on this backend, `required` for every other enabled cell.
fn cell_classification(cell: &SelectedCell) -> &'static str {
    if !cell.enabled {
        "disabled"
    } else if cell_diagnostic_reason(cell).is_some() {
        "diagnostic"
    } else {
        "required"
    }
}

/// The reason one cell's attempt history ends in a NON-BLOCKING diagnostic
/// failure, or `None` when it must count as an ordinary result.
///
/// Only a history whose LAST attempt is a measured FAIL of a `diagnostic`
/// cell qualifies: a product failure, or the cell exceeding its own time
/// budget (a timed-out compatibility probe was a product failure of its node
/// before these probes became cells). A PASS is a pass; an ERROR, or a FAIL
/// with any other no-result cause, keeps failing the run, including an ERROR
/// retry after a product FAIL, as does a diagnostic row that lost its
/// recorded reason.
pub fn diagnostic_failure_reason(history: &[CellResult]) -> Option<&str> {
    row_diagnostic_reason(history.last()?)
}

fn row_diagnostic_reason(result: &CellResult) -> Option<&str> {
    terminal_diagnostic_reason(
        &result.classification,
        &result.outcome,
        result.failure_class,
        result.result,
        &result.relaxations,
    )
}

/// The row-level rule behind [`diagnostic_failure_reason`], over the fields a
/// result reader has: a measured FAIL (a product failure, or a timeout) of a
/// `diagnostic` cell whose relaxations record exactly one non-blank
/// diagnostic reason. The scorecard applies it to the terminal attempt's row.
pub fn terminal_diagnostic_reason<'a>(
    classification: &str,
    outcome: &str,
    failure_class: Option<FailureClass>,
    result: Option<ObservedResult>,
    relaxations: &'a [String],
) -> Option<&'a str> {
    let measured = failure_class == Some(FailureClass::ProductFailure)
        || result == Some(ObservedResult::Timeout);
    if classification != "diagnostic" || outcome != "FAIL" || !measured {
        return None;
    }
    recorded_relaxation_reason(relaxations, DIAGNOSTIC_RELAXATION_PREFIX)
}

/// Whether relaxations record the stripped comparator with a reason.
pub fn records_stripped_comparator(relaxations: &[String]) -> bool {
    recorded_relaxation_reason(relaxations, STRIPPED_COMPARATOR_RELAXATION_PREFIX).is_some()
}

/// The reason of the one non-blank relaxation with `prefix`; `None` when there
/// is none, or more than one, since the runner records each at most once.
fn recorded_relaxation_reason<'a>(relaxations: &'a [String], prefix: &str) -> Option<&'a str> {
    let mut reasons = relaxations
        .iter()
        .filter_map(|relaxation| relaxation.strip_prefix(prefix))
        .filter(|reason| !reason.trim().is_empty());
    let reason = reasons.next()?;
    reasons.next().is_none().then_some(reason)
}

fn ci_selection(recipe: &ModeRecipe) -> Result<CiSelection, String> {
    CiSelection::validate(
        &recipe.backends_enabled.iter().cloned().collect(),
        &recipe.backends_disabled.keys().cloned().collect(),
        &recipe.ci,
        recipe.ci_disabled_reason.as_ref(),
    )
}

#[derive(Clone, Debug, Serialize)]
pub struct CellRunSpec {
    pub id: CellId,
    pub lane: String,
    pub category: String,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub timeout_seconds: u64,
    pub verdict_path: Option<PathBuf>,
    pub verification_log_dir: Option<PathBuf>,
    pub sabre_path_evidence: Option<PathBuf>,
    pub cell_dir: PathBuf,
    /// The exact nonzero guest disposition this attempt requires, if any.
    pub expected_guest_exit: Option<ExpectedGuestExit>,
    /// The exact stdout both compared verify runs must print, if declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_stdout: Option<String>,
    /// Text the captured verify stdout must contain, if declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_stdout_contains: Option<String>,
    /// The comparison a verify cell's report must pass; always `Strict` for
    /// every other mode.
    pub comparator: Comparator,
    #[serde(skip)]
    attempt: String,
    #[serde(skip)]
    fixed_workdir_source: PathBuf,
    /// Normalize the retained ptrace log into `normalized-ptrace-golden.log`.
    /// Only `E2E_KEEP_VERIFY_LOGS=1` asks for this; logs retained for the
    /// parity post-pass alone are never normalized, so parity retention adds
    /// no process to the cell and cannot change its outcome.
    #[serde(skip)]
    normalize_ptrace_golden: bool,
}

/// The existing pressure-test result vocabulary for one executed cell.
///
/// This is separate from [`FailureClass`]: two product failures can have
/// different results (`determinism-failure` and `crash-error`), while both are
/// still product failures. Keeping both values on the framework-written row
/// preserves that per-attempt distinction without asking pressure-test to
/// reconstruct it from process status and retained report files.
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
#[serde(rename_all = "kebab-case")]
pub enum ObservedResult {
    Pass,
    DeterminismFailure,
    ParityFailure,
    ReplayFailure,
    CrashError,
    Timeout,
    Oom,
    SandboxDenied,
    InfrastructureError,
}

impl ObservedResult {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pass" => Ok(Self::Pass),
            "determinism-failure" => Ok(Self::DeterminismFailure),
            "parity-failure" => Ok(Self::ParityFailure),
            "replay-failure" => Ok(Self::ReplayFailure),
            "crash-error" => Ok(Self::CrashError),
            "timeout" => Ok(Self::Timeout),
            "oom" => Ok(Self::Oom),
            "sandbox-denied" => Err(
                "pressure summary contains a sandbox-denied operation; refusing to store it as product behavior"
                    .into(),
            ),
            "infrastructure-error" => Err(
                "pressure summary contains an infrastructure error; refusing to store it as product behavior"
                    .into(),
            ),
            other => Err(format!("unknown pressure result `{other}`")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::DeterminismFailure => "determinism-failure",
            Self::ParityFailure => "parity-failure",
            Self::ReplayFailure => "replay-failure",
            Self::CrashError => "crash-error",
            Self::Timeout => "timeout",
            Self::Oom => "oom",
            Self::SandboxDenied => "sandbox-denied",
            Self::InfrastructureError => "infrastructure-error",
        }
    }

    pub fn carries_divergence_position(self) -> bool {
        matches!(
            self,
            Self::DeterminismFailure | Self::ParityFailure | Self::ReplayFailure
        )
    }

    pub fn failure_class(self) -> Option<FailureClass> {
        match self {
            Self::Pass => None,
            Self::DeterminismFailure
            | Self::ParityFailure
            | Self::ReplayFailure
            | Self::CrashError => Some(FailureClass::ProductFailure),
            Self::Timeout | Self::Oom => Some(FailureClass::NoResult),
            Self::SandboxDenied | Self::InfrastructureError => {
                Some(FailureClass::UnderstoodInfrastructureFailure)
            }
        }
    }
}

/// Attribution for a non-passing cell result.
///
/// These are the owner's four existing failure classes. `None` is reserved for
/// a pass or a retained row written before this field existed; every current
/// non-pass written by the framework carries one of these values.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    ProductFailure,
    UnderstoodInfrastructureFailure,
    UnderstoodPrerequisiteFailure,
    NoResult,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProductFailure => "product_failure",
            Self::UnderstoodInfrastructureFailure => "understood_infrastructure_failure",
            Self::UnderstoodPrerequisiteFailure => "understood_prerequisite_failure",
            Self::NoResult => "no_result",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "product_failure" => Ok(Self::ProductFailure),
            "understood_infrastructure_failure" => Ok(Self::UnderstoodInfrastructureFailure),
            "understood_prerequisite_failure" => Ok(Self::UnderstoodPrerequisiteFailure),
            "no_result" => Ok(Self::NoResult),
            other => Err(format!("unknown failure_class `{other}`")),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttemptResult {
    pub index: String,
    pub outcome: String,
    pub error_kind: Option<String>,
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(default)]
    pub duration_ms: u128,
    /// CPU consumed by the launched process group and any required retained-log
    /// normalization process.
    ///
    /// Completed commands use `wait4`; a CPU timeout retains the last live
    /// process-group observation when that is larger. It is not inferred from
    /// wall time and remains attributable when cells execute concurrently or
    /// move their work outside the enclosing DAG cgroup.
    #[serde(default)]
    pub cpu_usage_usec: Option<u64>,
    pub observation_sha256: Option<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub verification_report: Option<String>,
    pub verification_report_sha256: Option<String>,
    /// Runtime totals for the two executions compared by this attempt.
    pub runtime: Option<VerificationRuntime>,
    /// The divergence position, LIFTED OUT of `verification_report` so it is a
    /// field rather than a substring.
    ///
    /// `verification_report` already contained these numbers, but as a
    /// JSON-ENCODED STRING, so every consumer had to parse the row and then
    /// parse a string inside it. That is why no cell has ever reported where
    /// its divergence began: the value was present and unreachable. One
    /// attempt is one observation, so this is the level the position belongs
    /// at -- a cell with three attempts has three of them.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Companion to the field above, in virtual-nanosecond units.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// The coordinate that LOCATES the divergence rather than bounding it.
    pub first_divergent_record: Option<u64>,
    /// Syscalls the guest completed before diverging. A different keyspace from
    /// every other coordinate here; see the note in canonical_verdict.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the left execution, with only the
    /// separately recorded syscall number, scheduler turn, and committed time
    /// removed.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from the right execution.
    pub first_divergent_right_message: Option<String>,
    pub sabre_path_evidence: Option<String>,
    pub sabre_path_evidence_sha256: Option<String>,
    pub reason: Option<String>,
}

/// One test-harness cell observation written to `results.jsonl`.
///
/// This is not [`crate::ledger::CellResult`]. That type is the validation
/// ledger's compact cell verdict; this type is the test framework's complete
/// execution result, including invocation, attempts, timing, and retained
/// evidence.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CellResult {
    pub schema: u64,
    pub run_id: String,
    /// Short machine name measured with this row. A hostname alone is not a
    /// stable host class, but it is the existing per-machine key used by the
    /// parent store.
    #[serde(default)]
    pub machine_shortname: String,
    /// `uname -r` for the kernel under which this cell ran.
    #[serde(default)]
    pub kernel_version: String,
    /// The complete closed set of capability verdicts that determined which
    /// cells could execute on this host.
    #[serde(default)]
    pub host_capabilities: HostCapabilities,
    /// The cell attempt that produced this observation. A retry is a second
    /// observation, so it receives the next positive ordinal instead of
    /// replacing the earlier row.
    #[serde(default = "first_attempt")]
    pub attempt: u64,
    /// Pressure-test repetition number supplied to the test framework.
    /// Ordinary validate rows leave this absent and use `attempt` as their
    /// series run index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_index: Option<u64>,
    /// HEAD of the CHECKOUT the harness ran in. NOT the provenance of the
    /// binary that produced this measurement, despite the name.
    ///
    /// The harness cannot learn a binary's provenance from a path, and
    /// `hermit_bin` defaults to `target/debug/hermit` -- whatever file happens
    /// to be there. During any rebase-and-rerun loop the checkout moves and the
    /// binary does not, so this names a commit that never built the artifact.
    /// Read [`CellResult::binary_build_sha`] for that; read this only as "where
    /// the harness was standing".
    pub hermit_sha: String,
    /// Dirtiness of that same CHECKOUT at run time -- again not a statement
    /// about the binary, which may have been built from a different tree in a
    /// different state.
    pub source_tree_dirty: bool,
    pub binary_sha256: Option<String>,
    /// What the BINARY says about its own origin: the commit it was compiled
    /// from, with a `-dirty` suffix when that tree was dirty.
    ///
    /// `binary_sha256` proves WHICH file ran; this says WHERE THAT FILE CAME
    /// FROM. Together they are a complete attribution, and neither substitutes
    /// for the other. `None` means the binary could not be asked or did not
    /// answer recognisably -- deliberately distinct from a value, so "not
    /// established" never reads as agreement.
    pub binary_build_sha: Option<String>,
    #[serde(default)]
    pub test_sha256: String,
    pub test: String,
    pub category: String,
    pub lane: String,
    pub mode: String,
    pub backend: Option<String>,
    pub classification: String,
    pub outcome: String,
    /// What this cell attempt observed, using the same closed vocabulary that
    /// pressure summaries and the scorecard consume. `None` means either an
    /// infrastructure result or a retained row written before this field.
    #[serde(default)]
    pub result: Option<ObservedResult>,
    /// Who the non-pass is attributed to. This is deliberately separate from
    /// `error_kind`, which retains the more specific mechanism such as
    /// `backend-unavailable` or `incomplete-verification-evidence`.
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    pub error_kind: Option<String>,
    /// The cell wall-clock bound recorded by schema 4. It remains the fixture
    /// preparation and inner-invocation wall bound; the additive fields below
    /// describe the post-preparation policy without changing this field's
    /// meaning for existing readers.
    #[serde(default)]
    pub timeout_seconds: u64,
    /// Aggregate post-preparation CPU budget across attempts and seeds.
    /// Absent only on rows written before this policy became explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_cpu_timeout_seconds: Option<u64>,
    /// Outer post-preparation wall backstop for a near-zero-CPU wedge.
    /// Absent only on rows written before this policy became explicit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_wall_timeout_seconds: Option<u64>,
    /// Measured wall time for a cell that reached execution. Absent when the
    /// cell never ran; a measured zero remains a valid sub-millisecond result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u128>,
    /// CPU consumed by this cell's preparation, launched attempts, and parity
    /// comparison.
    ///
    /// Null when the cell never reached execution or the runner could not
    /// obtain a complete measurement. A measured zero remains a real value.
    #[serde(default)]
    pub cpu_usage_usec: Option<u64>,
    /// Per-invocation observations, independent of the charged aggregate.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::cpu_evidence::deserialize_present"
    )]
    pub cpu_observations: Option<crate::cpu_evidence::CellCpuObservationsV1>,
    /// Runtime totals from the first attempt that produced them.
    pub runtime: Option<VerificationRuntime>,
    pub log_level: Option<String>,
    #[serde(default)]
    pub effective_args: Vec<String>,
    pub argv: Vec<String>,
    pub guest_argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub shell_command: String,
    #[serde(default)]
    pub relaxations: Vec<String>,
    pub execution_path: Option<JsonValue>,
    pub diversity: Option<JsonValue>,
    pub attempts: Vec<AttemptResult>,
    /// Strict ptrace-reference comparison produced in this cell, when
    /// explicitly requested by the runner. Same-backend verification evidence
    /// remains in `attempts`; this field is the separate cross-backend fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_parity: Option<BackendParityReport>,
    /// Cell-level divergence position: the FIRST attempt that located one.
    ///
    /// This mirrors `reason` exactly, which is also
    /// `attempts.iter().find_map(...)`. Two other rules were available and both
    /// are worse here: the LAST attempt would let a passing retry erase the
    /// position a failing attempt found, and a min across attempts would be a
    /// one-run aggregate that reads like the cross-run `ObservedRange`
    /// earliest/latest without having a sample behind it. The per-attempt
    /// fields are still present, so a consumer that wants a different rule can
    /// apply it rather than being stuck with this one.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Companion to the field above, selected by the same first-attempt rule.
    /// Selected INDEPENDENTLY, so a report carrying only some of the three
    /// coordinates still contributes the ones it has.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// The coordinate that LOCATES the divergence. Same first-attempt rule.
    pub first_divergent_record: Option<u64>,
    /// Syscalls completed before diverging. Same first-attempt rule.
    pub first_divergent_syscall: Option<u64>,
    /// First differing compared message from the first attempt that recorded
    /// message content.
    pub first_divergent_left_message: Option<String>,
    /// Corresponding first differing compared message from that same attempt.
    pub first_divergent_right_message: Option<String>,
    pub reason: Option<String>,
    /// Where this cell's retained evidence lives.
    ///
    /// Carried on the result rather than recomputed by readers: the directory
    /// is `<result_root>/runs/<run_id>/<slug>` for attempt 1 and uses an
    /// `-attempt-N` suffix for a retry. The slug is built in exactly one place
    /// (`cell_artifact_dir`). A consumer that rebuilds it from `test`, `mode`,
    /// `backend`, and `attempt` becomes a second definition that goes stale
    /// silently, and a path that points at nothing is worse than no path at all.
    pub artifact_dir: String,
    /// The one nonzero guest disposition this verify cell declares.
    ///
    /// A matched verify attempt that ends with a nonzero status or a signal is
    /// indistinguishable, from the attempt alone, from a cell that crashed
    /// deterministically without saying it should. Recording the manifest's
    /// declaration here lets a reader admit exactly the declared disposition
    /// and keep refusing every other one. Absent for cells that declare none,
    /// so rows for ordinary cells are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_guest_exit: Option<ExpectedGuestExit>,
}

impl CellResult {
    /// Bind supplied CPU evidence to this row; historical absence adds no requirements.
    pub fn require_cpu_observations(&self) -> Result<(), String> {
        let Some(observations) = &self.cpu_observations else {
            return Ok(());
        };
        use crate::ledger::RequiredNullable;
        let binding = crate::cpu_evidence::CellCpuBinding {
            run_id: self.run_id.clone(),
            hermit_sha: self.hermit_sha.clone(),
            lane: self.lane.clone(),
            category: self.category.clone(),
            test: self.test.clone(),
            mode: self.mode.clone(),
            backend: self
                .backend
                .clone()
                .map_or(RequiredNullable::Null, RequiredNullable::Value),
            outer_attempt: self.attempt,
            run_index: self
                .run_index
                .map_or(RequiredNullable::Null, RequiredNullable::Value),
        };
        observations.validate_binding(&binding)?;
        for attempt in &self.attempts {
            observations.validate_attempt(
                &attempt.index,
                &attempt.argv,
                &attempt.cwd,
                &attempt.env,
            )?;
        }
        observations.require_passing_prerequisites(self.attempts.iter().map(|attempt| {
            (
                attempt.index.as_str(),
                attempt.outcome == "PASS"
                    && attempt.status == Some(0)
                    && attempt.signal.is_none()
                    && !attempt.timed_out,
                Some(attempt.timed_out),
            )
        }))?;
        Ok(())
    }

    /// Human-facing explanation without manufacturing a cause the producer did
    /// not record.
    pub fn reason_for_display(&self) -> &str {
        self.reason
            .as_deref()
            .unwrap_or(match self.outcome.as_str() {
                "FAIL" => "no specific failure reason was recorded",
                "ERROR" => "no specific error reason was recorded",
                "HOST-INAPPLICABLE" => "no prerequisite reason was recorded",
                _ => "no reason was recorded",
            })
    }

    /// Validate the additive timeout fields while keeping earlier schema-4 rows
    /// readable. `timeout_seconds` retains its original wall-bound meaning;
    /// only rows carrying both new fields claim the execution CPU/backstop
    /// policy.
    pub fn validate_timeout_policy(&self) -> Result<(), String> {
        match (
            self.execution_cpu_timeout_seconds,
            self.execution_wall_timeout_seconds,
        ) {
            (None, None) => Ok(()),
            (Some(cpu), Some(wall)) if cpu > 0 && wall == self.timeout_seconds && wall > cpu => {
                Ok(())
            }
            (cpu, wall) => Err(format!(
                "timeout policy disagrees: timeout_seconds={} execution_cpu_timeout_seconds={cpu:?} execution_wall_timeout_seconds={wall:?}",
                self.timeout_seconds
            )),
        }
    }

    /// Require evidence emitted by the current runner rather than a readable
    /// schema-4 row written before the additive timeout fields existed.
    pub fn require_current_timeout_policy(&self) -> Result<(), String> {
        self.validate_timeout_policy()?;
        if self.execution_cpu_timeout_seconds.is_none()
            || self.execution_wall_timeout_seconds.is_none()
        {
            return Err("current result omitted explicit execution timeout bounds".into());
        }
        Ok(())
    }

    /// Check the classification fields written by the current framework.
    ///
    /// Both fields remain optional in the deserializer because schema 4 also
    /// names retained rows written before they existed. Current publication is
    /// stricter: every result is recorded, and every non-pass is attributed.
    pub fn require_current_classification(&self) -> Result<(), String> {
        match self.outcome.as_str() {
            "PASS" => {
                if self.result != Some(ObservedResult::Pass) || self.failure_class.is_some() {
                    return Err(format!(
                        "PASS result must carry result=pass and no failure_class, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "HOST-INAPPLICABLE" => {
                if self.result.is_some()
                    || self.failure_class != Some(FailureClass::UnderstoodPrerequisiteFailure)
                {
                    return Err(format!(
                        "HOST-INAPPLICABLE result must carry understood_prerequisite_failure and no observed result, got result={:?} failure_class={:?}",
                        self.result, self.failure_class
                    ));
                }
            }
            "FAIL" => {
                let failure_class = self.failure_class.ok_or_else(|| {
                    format!(
                        "{} result has no failure_class; current non-passes must be attributed",
                        self.outcome
                    )
                })?;
                let result = self.result.ok_or_else(|| {
                    "FAIL result has no observed result; the failure kind was lost".to_string()
                })?;
                let expected = result.failure_class().ok_or_else(|| {
                    format!("FAIL result cannot carry the passing result {result:?}")
                })?;
                if failure_class != expected {
                    return Err(format!(
                        "observed result {} requires failure_class {:?}, got {:?}",
                        result.as_str(),
                        expected,
                        failure_class
                    ));
                }
            }
            "ERROR" => match (self.result, self.failure_class) {
                (
                    Some(ObservedResult::SandboxDenied | ObservedResult::InfrastructureError),
                    Some(FailureClass::UnderstoodInfrastructureFailure),
                ) => {}
                (
                    None,
                    Some(
                        FailureClass::UnderstoodInfrastructureFailure
                        | FailureClass::UnderstoodPrerequisiteFailure
                        | FailureClass::NoResult,
                    ),
                )
                | (
                    Some(ObservedResult::Timeout | ObservedResult::Oom),
                    Some(FailureClass::NoResult),
                ) => {}
                other => {
                    return Err(format!(
                        "ERROR result must carry a non-product observation/classification, got {other:?}"
                    ));
                }
            },
            other => return Err(format!("unknown cell outcome {other:?}")),
        }
        Ok(())
    }

    /// Retained schema-4 rows predate these fields. Accept that exact legacy
    /// absence, but validate any row that carries either half of the contract.
    pub fn validate_recorded_classification(&self) -> Result<(), String> {
        if self.result.is_none() && self.failure_class.is_none() {
            return Ok(());
        }
        self.require_current_classification()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledWorkerCapacity(usize);

impl ScheduledWorkerCapacity {
    pub fn new(configured: usize) -> Self {
        assert!(configured > 0, "scheduled worker capacity must be positive");
        Self(configured)
    }

    pub fn configured(self) -> usize {
        self.0
    }

    pub fn workers_for(self, count: usize) -> usize {
        self.0.min(count)
    }
}

#[derive(Clone)]
pub struct RunContext {
    pub root: PathBuf,
    pub hermit_bin: PathBuf,
    pub result_root: PathBuf,
    pub build_root: PathBuf,
    pub run_id: String,
    pub machine_shortname: String,
    pub kernel_version: String,
    pub host_capabilities: HostCapabilities,
    pub attempt: u64,
    pub run_index: Option<u64>,
    /// One concrete CLI input shared by comparison operands and outer retries.
    pub epoch: String,
    pub source_sha: String,
    pub source_dirty: bool,
    /// Provenance the hermit binary reports about itself, probed once per run.
    /// See [`CellResult::binary_build_sha`].
    pub binary_build_sha: Option<String>,
    pub prebuilt: bool,
    /// `E2E_KEEP_VERIFY_LOGS=1`: retain every verify cell's logs and normalize
    /// each ptrace golden, as before.
    pub keep_logs: bool,
    /// `(test, backend)` verify cells whose logs are retained for the parity
    /// post-pass even without `keep_logs`. This is the selection closure of
    /// [`crate::parity::retention_closure`]: retention only, so it neither
    /// normalizes a ptrace golden nor adds any other process to the cell.
    pub parity_retained: BTreeSet<(String, String)>,
    pub run_verify_strict: bool,
    pub record_verify_strict: bool,
    /// Per-machine scaling for individual test CPU and wall bounds. This is execution policy,
    /// not part of the canonical manifest.
    pub timeout_multipliers: TimeoutMultipliers,
    pub scheduled_worker_capacity: ScheduledWorkerCapacity,
    pub isolated_workdir: Option<PathBuf>,
}

fn resolve_run_epoch(
    explicit: Option<std::ffi::OsString>,
    capture_now: impl FnOnce() -> SystemTime,
) -> Result<String, String> {
    match explicit {
        // Preserve the supported input exactly, including invalid values that
        // the CLI must reject. Never replace an override with a new sample.
        Some(epoch) => epoch
            .into_string()
            .map_err(|_| "HERMIT_EPOCH must be valid Unicode".to_owned()),
        None => Ok(detcore_model::config::epoch_from_host_time(capture_now()).to_rfc3339()),
    }
}

/// The `HERMIT_EPOCH` a harness process started now would give every guest:
/// the environment's value verbatim, or one sampled from host time. A caller
/// that launches several harness processes whose logs are compared (the
/// pressure test's parity post-pass) samples this once and passes it to each.
pub fn run_epoch_from_env() -> Result<String, String> {
    resolve_run_epoch(std::env::var_os("HERMIT_EPOCH"), SystemTime::now)
}

impl RunContext {
    /// A copy of this context whose result row and retained artifacts use the
    /// given cell-attempt ordinal.
    pub fn with_attempt(&self, attempt: u64) -> Self {
        Self {
            attempt,
            ..self.clone()
        }
    }

    /// `source_sha` names the commit a Git-less source snapshot at `root` was
    /// archived from (`--source-sha`); without it, Git describes the checkout.
    pub fn from_env(
        root: PathBuf,
        prebuilt: bool,
        source_sha: Option<&str>,
    ) -> Result<Self, String> {
        let (source_sha, source_dirty) = source_identity(&root, source_sha)?;
        let result_root = std::env::var_os("E2E_RESULT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("ignored/e2e"));
        let run_id = std::env::var("E2E_RUN_ID").unwrap_or_else(|_| {
            format!(
                "local-{}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                std::process::id()
            )
        });
        let run_index = std::env::var(E2E_RUN_INDEX_ENV)
            .ok()
            .map(|value| {
                value.parse::<u64>().map_err(|_| {
                    format!("{E2E_RUN_INDEX_ENV} must be a non-negative integer, got {value:?}")
                })
            })
            .transpose()?;
        let machine_name = match std::env::var(E2E_MACHINE_SHORTNAME_ENV) {
            Ok(value) if !value.is_empty() => value,
            _ => command_text("hostname", &["-s"])
                .or_else(|_| command_text("hostname", &[]))
                .map_err(|error| format!("cannot establish machine_shortname: {error}"))?,
        };
        let machine_shortname = machine_name
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string();
        if machine_shortname.is_empty() || machine_shortname.contains('/') {
            return Err(format!(
                "machine_shortname must be one nonempty path segment, got {machine_shortname:?}"
            ));
        }
        let kernel_version = match std::env::var(E2E_KERNEL_VERSION_ENV) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => command_text("uname", &["-r"])
                .map_err(|error| format!("cannot establish kernel_version: {error}"))?,
        };
        let host_capabilities = probe_host_capabilities();
        let build_root = std::env::var_os("E2E_BUILD_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| result_root.join("build").join(&source_sha));
        let hermit_bin = resolve_hermit_bin(&root, std::env::var_os("HERMIT_BIN"));
        // Ask the binary where it came from, the same way this function already
        // asks it what flags it supports. `source_sha` above describes the
        // checkout; only the binary can describe the binary.
        let binary_build_sha = probe_binary_build_sha(&hermit_bin);
        // Published main still exposes the legacy `--verify-strict` spelling;
        // the canonical-only cutover removes it and makes bare `--verify`
        // canonical.  Detect the running binary rather than keying behavior to
        // a source SHA.  Whichever spelling executes is retained verbatim in
        // the result row, so this bridge cannot hide the comparison policy.
        let run_verify_strict =
            command_help_contains(&hermit_bin, &["run", "--help"], "--verify-strict")?;
        let isolated_workdir = match std::env::var_os(ISOLATED_WORKDIR_ENV) {
            None => None,
            Some(value) if value == HERMETIC_TEST_WORKDIR => {
                Some(PathBuf::from(HERMETIC_TEST_WORKDIR))
            }
            Some(value) => {
                return Err(format!(
                    "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {}",
                    PathBuf::from(value).display()
                ));
            }
        };
        let record_verify_strict = command_help_contains(
            &hermit_bin,
            &["record", "start", "--help"],
            "--verify-strict",
        )?;
        Ok(Self {
            root,
            hermit_bin,
            result_root,
            build_root,
            run_id,
            machine_shortname,
            kernel_version,
            host_capabilities,
            attempt: 1,
            run_index,
            epoch: resolve_run_epoch(std::env::var_os("HERMIT_EPOCH"), SystemTime::now)?,
            source_sha,
            source_dirty,
            binary_build_sha,
            prebuilt,
            keep_logs: std::env::var("E2E_KEEP_VERIFY_LOGS").as_deref() == Ok("1"),
            parity_retained: BTreeSet::new(),
            run_verify_strict,
            record_verify_strict,
            timeout_multipliers: timeout_multipliers_from_env()?,
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir,
        })
    }

    /// Retain the verify logs of these `(test, backend)` cells for the parity
    /// post-pass. See [`RunContext::parity_retained`].
    pub fn with_parity_retained(mut self, cells: BTreeSet<(String, String)>) -> Self {
        self.parity_retained = cells;
        self
    }

    /// Whether this verify cell's logs are kept after the run.
    pub fn retains_verify_logs(&self, test: &str, backend: &str) -> bool {
        self.keep_logs
            || self
                .parity_retained
                .contains(&(test.to_string(), backend.to_string()))
    }

    pub fn with_scheduled_worker_capacity(
        mut self,
        scheduled_worker_capacity: ScheduledWorkerCapacity,
    ) -> Self {
        self.scheduled_worker_capacity = scheduled_worker_capacity;
        self
    }
}

/// One initial execution plus one retry, scoped to one selected cell.
pub const MAX_ATTEMPTS_PER_CELL: u64 = 2;

/// Select the outcome that the framework reports for one cell's complete
/// attempt history.
///
/// A passing retry makes the cell pass, while the appended rows still retain
/// the failure it recovered from. If no attempt passes, any product failure
/// keeps the cell failed even when a later attempt ends in an infrastructure
/// error. A history containing only infrastructure errors remains an error.
/// Host-inapplicable is terminal and cannot be mixed with executed attempts.
pub fn outcome_after_retries<'a>(
    attempts: impl IntoIterator<Item = (u64, &'a str)>,
) -> Result<&'static str, String> {
    let mut expected_attempt = 1;
    let mut saw_pass = false;
    let mut saw_failure = false;
    let mut saw_error = false;
    let mut saw_host_inapplicable = false;

    for (attempt, outcome) in attempts {
        if attempt > MAX_ATTEMPTS_PER_CELL {
            return Err(format!(
                "cell result attempt {attempt} exceeds the shared maximum of {MAX_ATTEMPTS_PER_CELL}"
            ));
        }
        if attempt != expected_attempt {
            return Err(format!(
                "cell result attempt {attempt} does not follow the preceding attempts; expected {expected_attempt}"
            ));
        }
        if saw_pass || saw_host_inapplicable {
            return Err(format!(
                "cell result attempt {attempt} follows terminal outcome {}",
                if saw_pass {
                    "PASS"
                } else {
                    "HOST-INAPPLICABLE"
                }
            ));
        }
        match outcome {
            "PASS" => saw_pass = true,
            "FAIL" => saw_failure = true,
            "ERROR" => saw_error = true,
            "HOST-INAPPLICABLE" => saw_host_inapplicable = true,
            other => {
                return Err(format!(
                    "cell result attempt {attempt} has unknown outcome {other:?}"
                ));
            }
        }
        expected_attempt += 1;
    }

    if expected_attempt == 1 {
        return Err("cell result has no attempts".into());
    }
    if saw_host_inapplicable && (saw_failure || saw_error) {
        return Err("HOST-INAPPLICABLE cannot be mixed with executed cell attempts".into());
    }
    Ok(if saw_pass {
        "PASS"
    } else if saw_failure {
        "FAIL"
    } else if saw_error {
        "ERROR"
    } else {
        "HOST-INAPPLICABLE"
    })
}

/// Return the framework result row that carries the outcome selected by
/// [`outcome_after_retries`]. All attempt rows remain in `results.jsonl`; this
/// row is the one used for the cell's JUnit and summary entry.
pub fn cell_result_after_retries(results: &[CellResult]) -> Result<&CellResult, String> {
    let outcome = outcome_after_retries(
        results
            .iter()
            .map(|result| (result.attempt, result.outcome.as_str())),
    )?;
    results
        .iter()
        .rev()
        .find(|result| result.outcome == outcome)
        .ok_or_else(|| format!("cell result history selected {outcome} without a matching row"))
}

/// Return both the selected result row and the number of attempts in its
/// validated history. The selected row keeps its own ordinal and artifact;
/// the separate count is what structured summaries report.
pub fn cell_result_and_attempts_after_retries(
    results: &[CellResult],
) -> Result<(&CellResult, u64), String> {
    let selected = cell_result_after_retries(results)?;
    let attempts = results
        .last()
        .map(|result| result.attempt)
        .ok_or_else(|| "cell result has no attempts".to_string())?;
    Ok((selected, attempts))
}

fn command_text(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot execute {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!("{program} exited {}", output.status));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|error| format!("{program} output is not UTF-8: {error}"))?
        .trim()
        .to_string();
    if value.is_empty() {
        return Err(format!("{program} returned an empty value"));
    }
    Ok(value)
}

pub fn prepare_test(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
) -> Result<Vec<String>, String> {
    let timeouts = cell_timeouts(context, cell)?;
    prepare_test_until(
        context,
        cell,
        dir,
        execution_deadline_after_preparation(Instant::now(), timeouts.wall_seconds)?,
        timeouts.wall_seconds,
        &mut Vec::new(),
    )
    .map(|(guest, _cpu_usage_usec)| guest)
}

fn prepare_test_until(
    context: &RunContext,
    cell: &SelectedCell,
    dir: &Path,
    deadline: Instant,
    wall_timeout_seconds: u64,
    observations: &mut Vec<InvocationCpuObservation>,
) -> Result<(Vec<String>, u64), String> {
    prepare_dirs(&context.root, dir)?;
    let mut cpu_usage_usec = 0u64;
    if context.prebuilt && cell.test.program.is_some() {
        let source = context
            .build_root
            .join(cell.test.id.replace('/', "-"))
            .join("fixtures");
        if !source.is_dir() {
            return Err(format!("prebuilt fixture is missing: {}", source.display()));
        }
        let destination = dir.join("fixtures");
        if destination.exists() {
            fs::remove_dir_all(&destination)
                .map_err(|e| format!("cannot clear {}: {e}", destination.display()))?;
        }
        copy_tree(&source, &destination)?;
    }
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let mode = cell.test.modes.get(&cell.id.mode).unwrap();
    let guest_args = mode.guest_args.get(backend).cloned().unwrap_or_default();
    let mut guest = match (&cell.test.program, &cell.test.direct) {
        (Some(program), None) if program.ends_with(".c") => {
            let output = dir.join("fixtures/program");
            if !context.prebuilt {
                let _ = fs::remove_file(&output);
                let mut args = vec!["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"]
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                args.extend(
                    cell.test
                        .build
                        .as_ref()
                        .map(|b| b.cflags.clone())
                        .unwrap_or_default(),
                );
                args.push(context.root.join(program).to_string_lossy().into_owned());
                args.push("-o".into());
                args.push(output.to_string_lossy().into_owned());
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        "cc",
                        &args,
                        deadline,
                        wall_timeout_seconds,
                        observations,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".rs") => {
            let output = dir.join("fixtures/program");
            if !context.prebuilt {
                let _ = fs::remove_file(&output);
                let mut args = vec!["-O".to_string()];
                args.extend(
                    cell.test
                        .build
                        .as_ref()
                        .map(|b| b.rustflags.clone())
                        .unwrap_or_default(),
                );
                args.push(context.root.join(program).to_string_lossy().into_owned());
                args.push("-o".into());
                args.push(output.to_string_lossy().into_owned());
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        "rustc",
                        &args,
                        deadline,
                        wall_timeout_seconds,
                        observations,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            require_executable_program(&output, &dir.join("captures"))?;
            vec![output.to_string_lossy().into_owned()]
        }
        (Some(program), None) if program.ends_with(".sh") => {
            let path = context.root.join(program).to_string_lossy().into_owned();
            if !context.prebuilt {
                cpu_usage_usec = cpu_usage_usec
                    .checked_add(run_preparation(
                        context,
                        dir,
                        &path,
                        &["--prepare".into()],
                        deadline,
                        wall_timeout_seconds,
                        observations,
                    )?)
                    .ok_or_else(|| "cell CPU usage overflowed u64".to_string())?;
            }
            vec![path, "--run".into()]
        }
        (None, Some(DirectCommand::Shell(command))) => {
            let mut argv = vec!["bash".into(), "-c".into(), command.clone()];
            if !guest_args.is_empty() {
                argv.push("--".into());
            }
            argv
        }
        (None, Some(DirectCommand::Argv(argv))) => {
            crate::manifest_corpus::resolve_direct_placeholders(
                &cell.test.id,
                argv,
                &context.root,
                crate::manifest_corpus::validate_run_state().as_deref(),
            )?
        }
        _ => return Err(format!("{} has unsupported program kind", cell.test.id)),
    };
    guest.extend(guest_args);
    if context.isolated_workdir.is_some()
        || (backend != "dbt" && supports_test_workdir(&cell.id.mode, backend))
    {
        resolve_repo_guest_args(&context.root, &mut guest);
    }
    Ok((guest, cpu_usage_usec))
}

fn resolve_repo_guest_args(root: &Path, argv: &mut [String]) {
    for arg in argv {
        let path = Path::new(arg);
        if path.is_absolute() || arg == "." || arg == ".." {
            continue;
        }
        let resolved = root.join(path);
        let looks_like_repo_path = arg.starts_with("./") || arg.contains('/') || resolved.is_file();
        if looks_like_repo_path && resolved.exists() {
            *arg = resolved.to_string_lossy().into_owned();
        }
    }
}

/// What the preparation child actually said, stderr first.
///
/// The child's output is already captured to `prepare.stderr` / `prepare.stdout`;
/// this is the only thing that carries it back to the caller. It matters beyond
/// readability: validate classifies an environmentally-blocked gate by reading
/// the FAILING NODE's own output region and nothing else, deliberately, so that
/// an unrelated concurrent host denial cannot excuse a real red. A build step
/// that swallows its compiler's stderr therefore hides the very evidence the
/// classifier is looking for, and a host denial gets recorded as a product
/// failure. Measured 2026-08-17 in one cold run: `build.manifest_guests`
/// reported three guests as "missing or not executable" with no diagnostic while
/// the jailer had denied their `cc`/`ld`, and `build.runtime_release` in the SAME
/// run propagated its banner, was classified `bpfjailer-banner`, and was retried.
fn preparation_diagnostic(captures: &Path) -> String {
    fs::read_to_string(captures.join("prepare.stderr"))
        .ok()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(captures.join("prepare.stdout"))
                .ok()
                .filter(|text| !text.trim().is_empty())
        })
        .unwrap_or_default()
}

/// Append a captured diagnostic to `message`, or say plainly that there was none.
///
/// The empty case is spelled out rather than left blank: "no output was captured"
/// is a different and much more suspicious fact than a compiler error, and a
/// reader who cannot tell them apart cannot tell a denial from a broken guest.
fn with_diagnostic(message: String, captures: &Path) -> String {
    let diagnostic = preparation_diagnostic(captures);
    if diagnostic.is_empty() {
        format!(
            "{message}\n  (no output was captured in {}; the preparation command produced none)",
            captures.display()
        )
    } else {
        format!("{message}\n{diagnostic}")
    }
}

fn remaining_cell_time(deadline: Instant) -> Duration {
    remaining_cell_time_at(deadline, Instant::now())
}

fn remaining_cell_seconds(deadline: Instant) -> u64 {
    let remaining = remaining_cell_time(deadline);
    if remaining.is_zero() {
        return 1;
    }
    remaining
        .as_secs()
        .saturating_add(u64::from(remaining.subsec_nanos() != 0))
}

fn remaining_cell_time_at(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

pub fn resolved_cell_timeouts(
    cell: &SelectedCell,
    timeout_multipliers: TimeoutMultipliers,
) -> Result<ResolvedTestTimeouts, String> {
    resolve_test_timeouts(
        cell.cpu_timeout_seconds,
        cell.timeout_seconds,
        timeout_multipliers,
    )
}

fn cell_timeouts(
    context: &RunContext,
    cell: &SelectedCell,
) -> Result<ResolvedTestTimeouts, String> {
    resolved_cell_timeouts(cell, context.timeout_multipliers)
}

fn execution_deadline_after_preparation(
    prepared_at: Instant,
    wall_timeout_seconds: u64,
) -> Result<Instant, String> {
    prepared_at
        .checked_add(Duration::from_secs(wall_timeout_seconds))
        .ok_or_else(|| format!("wall timeout {wall_timeout_seconds}s exceeds clock range"))
}

fn require_executable_program(path: &Path, captures: &Path) -> Result<(), String> {
    let executable = path
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if executable {
        return Ok(());
    }
    Err(with_diagnostic(
        format!(
            "compiled guest is missing or not executable: {}",
            path.display()
        ),
        captures,
    ))
}

fn run_preparation(
    context: &RunContext,
    dir: &Path,
    program: &str,
    args: &[String],
    deadline: Instant,
    cell_timeout_seconds: u64,
    observations: &mut Vec<InvocationCpuObservation>,
) -> Result<u64, String> {
    let captures = dir.join("captures");
    let request = ProcessRequest::new(
        InvocationRole::Preparation,
        &context.root,
        program,
        args,
        &preparation_env(dir),
        &captures.join("prepare.stdout"),
        &captures.join("prepare.stderr"),
    );
    if remaining_cell_time(deadline).is_zero() {
        record_not_started(
            request,
            false,
            NotStartedReason::WallBudgetAlreadyExhausted,
            observations,
        );
        return Err(with_diagnostic(
            format!("cell exceeded {cell_timeout_seconds} s during fixture preparation"),
            &captures,
        ));
    }
    let output = execute_process(request, (deadline, None), observations)?;
    if output.timeout.is_some() || !output.status.success() {
        // Carry the child's own words back. This used to return the bare sentence
        // and drop `prepare.stderr` on the floor, which turned every denied or
        // broken compile into the same uninformative line.
        let how = if output.timeout.is_some() {
            format!("cell exceeded {cell_timeout_seconds} s during fixture preparation")
        } else {
            match output.status.code() {
                Some(code) => format!("exited {code}"),
                None => "was killed by a signal".to_string(),
            }
        };
        return Err(with_diagnostic(
            format!("fixture preparation failed for {program}: {how}"),
            &captures,
        ));
    }
    Ok(output.cpu_usage_usec)
}

pub fn build_spec(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    guest_argv: Vec<String>,
    attempt: &str,
    seed: Option<i64>,
    timeout_seconds: u64,
) -> Result<CellRunSpec, String> {
    // The label is used in verdict, log, evidence, and capture paths. Reject
    // traversal and multi-component values before any of those paths are
    // created. The ordinary host-bound workdir uses the same guard; the
    // hermetic tmpfs path does not remove this path-safety property.
    let fixed_workdir_source = fixed_workdir_source_for_attempt(&dir, attempt)?;
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    let equalized_inputs = equalizes_guest_inputs(&cell.id.mode, backend);
    // The recorded guest argv is the argv after `--`, so both carry the guest
    // paths of an equalized cell.
    let guest_argv = if equalized_inputs {
        equalized_guest_argv(&dir, guest_argv)
    } else {
        guest_argv
    };
    let mode_recipe = &cell.test.modes[&cell.id.mode];
    let mut env = execution_cell_env(context, &dir, cell.id.mode != "naked");
    if cell.id.mode != "naked" {
        // An environment input preserves explicit custom --epoch precedence.
        // append_guest_env_args intentionally does not forward this CLI input.
        env.insert("HERMIT_EPOCH".into(), context.epoch.clone());
    }
    let sabre_path_evidence = (backend == "sabre").then(|| {
        dir.join("captures")
            .join(format!("{}-{attempt}.sabre-path.jsonl", cell.id.mode))
    });
    if let Some(path) = &sabre_path_evidence {
        fs::create_dir_all(path.parent().expect("evidence path has a parent"))
            .map_err(|e| e.to_string())?;
        fs::write(path, b"").map_err(|e| e.to_string())?;
        env.insert(
            "HERMIT_SABRE_PATH_EVIDENCE".into(),
            path.to_string_lossy().into_owned(),
        );
    }
    // The explicit hermetic path is available only where the launched backend
    // can honour a guest working directory. Replay is included there because
    // `record start` accepts the same mount and workdir arguments as `run`.
    let supports_test_workdir = supports_test_workdir(&cell.id.mode, backend);
    if context.isolated_workdir.is_some() && !supports_test_workdir {
        return Err(format!(
            "hermetic validation cannot isolate a {} cell on the {backend} backend at /test",
            cell.id.mode
        ));
    }
    let bound_workdir_source = (matches!(cell.id.mode.as_str(), "verify" | "chaos" | "custom")
        && backend != "dbt")
        .then_some(fixed_workdir_source.as_path());
    let isolated = context.isolated_workdir.is_some();
    let verdict = dir.join(format!("verify-{attempt}.json"));
    let mut verification_log_dir = None;
    let (argv, verdict_path) = match cell.id.mode.as_str() {
        "naked" => (guest_argv.clone(), None),
        "verify" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "--backend".into(),
                backend.into(),
                "run".into(),
                "--base-env=minimal".into(),
                "--strict".into(),
            ];
            if context.run_verify_strict && mode_recipe.comparator != Some(Comparator::Stripped) {
                argv.push("--verify-strict".into());
            }
            argv.extend(mode_recipe.hermit_args.iter().cloned());
            if mode_recipe.compare_io_buffers == Some(false) {
                argv.push("--no-detlog-io-buffers".into());
            }
            if mode_recipe.rcb_time == Some(false) {
                argv.push("--no-rcb-time".into());
            }
            argv.push("--verify".into());
            if mode_recipe.expected_guest_exit.is_some() {
                // The exact disposition is enforced on the report after the
                // comparison; this only lets a failing first run be compared.
                argv.push("--verify-allow=failure".into());
            }
            argv.extend([
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
            ]);
            if context.retains_verify_logs(&cell.id.test, backend) {
                let logs = dir.join(format!("verify-logs/verify-{attempt}"));
                fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
                argv.extend([
                    "--keep-logs".into(),
                    "--verify-log-dir".into(),
                    logs.to_string_lossy().into_owned(),
                ]);
                verification_log_dir = Some(logs);
            }
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            if equalized_inputs {
                append_equalized_input_binds(&mut argv, &dir);
                append_guest_env_args(&mut argv, &equalized_guest_env(&env), isolated);
            } else {
                append_guest_env_args(&mut argv, &env, isolated);
            }
            for (name, value) in &mode_recipe.env {
                argv.extend(["--env".into(), format!("{name}={value}")]);
            }
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "replay" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "--backend".into(),
                backend.into(),
                "record".into(),
                "start".into(),
                "--strict".into(),
            ];
            if isolated {
                argv.push("--base-env=minimal".into());
            }
            if context.record_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
                "--data-dir".into(),
                dir.join("recording").to_string_lossy().into_owned(),
                "--record-timeout".into(),
                timeout_seconds.to_string(),
            ]);
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "chaos" => {
            let seed = seed.ok_or_else(|| "chaos attempt requires a seed".to_string())?;
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "--backend".into(),
                backend.into(),
                "run".into(),
                "--base-env=minimal".into(),
                "--strict".into(),
            ];
            if context.run_verify_strict {
                argv.push("--verify-strict".into());
            }
            argv.extend([
                "--verify".into(),
                "--verify-allow=both".into(),
                "--verify-json".into(),
                verdict.to_string_lossy().into_owned(),
                "--chaos".into(),
                "--sched-heuristic=random".into(),
                format!("--seed={seed}"),
            ]);
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            append_guest_env_args(&mut argv, &env, isolated);
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, Some(verdict))
        }
        "custom" => {
            let mut argv = vec![
                context.hermit_bin.to_string_lossy().into_owned(),
                "--log".into(),
                "info".into(),
                "--backend".into(),
                backend.into(),
                "run".into(),
            ];
            argv.extend(cell.test.modes["custom"].args.clone());
            if isolated {
                require_minimal_base_env(&mut argv)?;
                append_guest_env_args(&mut argv, &env, true);
            } else {
                append_scheduled_jobs_env_arg(&mut argv, &env);
            }
            append_execution_root_args(
                &mut argv,
                backend,
                context.isolated_workdir.as_deref(),
                mode_recipe.workdir.as_deref(),
                bound_workdir_source,
            );
            argv.push("--".into());
            argv.extend(guest_argv.clone());
            (argv, None)
        }
        mode => return Err(format!("unsupported mode {mode}")),
    };
    Ok(CellRunSpec {
        id: cell.id.clone(),
        lane: cell.test.lane.clone(),
        category: cell.category.clone(),
        cwd: context.root.clone(),
        env,
        argv,
        guest_argv,
        timeout_seconds,
        verdict_path,
        verification_log_dir,
        sabre_path_evidence,
        cell_dir: dir,
        expected_guest_exit: cell_expected_guest_exit(cell),
        expected_stdout: cell_expected_stdout(cell),
        expected_stdout_contains: cell_expected_stdout_contains(cell),
        comparator: if cell.id.mode == "verify" {
            mode_recipe.comparator.unwrap_or(Comparator::Strict)
        } else {
            Comparator::Strict
        },
        attempt: attempt.into(),
        fixed_workdir_source,
        normalize_ptrace_golden: context.keep_logs,
    })
}

fn current_verification_report(bytes: &[u8]) -> Result<VerificationReport, String> {
    VerificationReport::from_current_json_slice(bytes)
}
pub fn execute_spec(spec: &CellRunSpec) -> Result<AttemptResult, String> {
    execute_spec_until(
        spec,
        execution_deadline_after_preparation(Instant::now(), spec.timeout_seconds)?,
        spec.timeout_seconds,
        spec.timeout_seconds,
        None,
        &mut Vec::new(),
    )
}

fn execute_spec_until(
    spec: &CellRunSpec,
    deadline: Instant,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    remaining_cpu_usec: Option<u64>,
    observations: &mut Vec<InvocationCpuObservation>,
) -> Result<AttemptResult, String> {
    // The attempt label comes from the spec rather than a parallel parameter.
    // `build_spec` already stored it, and every caller passed the same value to
    // both, so a separate argument was a second definition that could go stale
    // against the workdir and capture paths built from the first.
    let index = spec.attempt.as_str();
    if spec.argv.is_empty() {
        return Err("empty cell argv".into());
    }
    if let Some(verdict) = &spec.verdict_path {
        let _ = fs::remove_file(verdict);
    }
    let tmp = spec.cell_dir.join("tmp");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).map_err(|e| format!("cannot reset {}: {e}", tmp.display()))?;
    }
    fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    // The ordinary-host fallback binds this exact per-attempt directory at
    // /tmp/test. The explicit hermetic path mounts a fresh tmpfs at /test and
    // ignores the directory, but keeping the reset unconditional preserves the
    // fallback and makes switching paths a pure precedence decision.
    let workdir = &spec.fixed_workdir_source;
    if workdir.exists() {
        fs::remove_dir_all(workdir)
            .map_err(|e| format!("cannot reset {}: {e}", workdir.display()))?;
    }
    fs::create_dir_all(workdir).map_err(|e| format!("cannot create {}: {e}", workdir.display()))?;
    let captures = spec.cell_dir.join("captures");
    fs::create_dir_all(&captures).map_err(|e| e.to_string())?;
    let stdout_path = captures.join(format!("{}-{index}.stdout", spec.id.mode));
    let stderr_path = captures.join(format!("{}-{index}.stderr", spec.id.mode));
    let request = ProcessRequest::new(
        InvocationRole::Execution {
            attempt_index: spec.attempt.clone(),
            backend: spec
                .id
                .backend
                .clone()
                .map_or(RequiredNullable::Null, RequiredNullable::Value),
        },
        &spec.cwd,
        &spec.argv[0],
        &spec.argv[1..],
        &spec.env,
        &stdout_path,
        &stderr_path,
    );
    let started = Instant::now();
    let remaining = remaining_cell_time(deadline);
    let exhausted = if remaining.is_zero() {
        Some(ProcessTimeout::Wall)
    } else if remaining_cpu_usec == Some(0) {
        Some(ProcessTimeout::Cpu)
    } else {
        None
    };
    if let Some(timeout) = exhausted {
        record_not_started(
            request,
            remaining_cpu_usec.is_some(),
            match timeout {
                ProcessTimeout::Cpu => NotStartedReason::CpuBudgetAlreadyExhausted,
                ProcessTimeout::Wall => NotStartedReason::WallBudgetAlreadyExhausted,
            },
            observations,
        );
        fs::write(&stdout_path, b"").map_err(|e| e.to_string())?;
        fs::write(&stderr_path, b"").map_err(|e| e.to_string())?;
        return Ok(cell_timeout_attempt(
            spec,
            cpu_timeout_seconds,
            wall_timeout_seconds,
            started.elapsed(),
            timeout,
        ));
    }
    let execution_ordinal = observations.len() as u64 + 1;
    let output = execute_process(request, (deadline, remaining_cpu_usec), observations)?;
    let mut accounted_cpu_usage_usec = Some(output.cpu_usage_usec);
    let mut normalization_error = None;
    if output.timeout.is_none()
        && spec.normalize_ptrace_golden
        && spec.id.mode == "verify"
        && spec.id.backend.as_deref() == Some("ptrace")
    {
        if let Some(directory) = &spec.verification_log_dir {
            let normalized = normalize_ptrace_golden(
                &spec.argv[0],
                directory,
                &spec.cwd,
                deadline,
                remaining_cpu_usec.map(|budget| budget.saturating_sub(output.cpu_usage_usec)),
                (cpu_timeout_seconds, wall_timeout_seconds),
                (
                    observations,
                    InvocationRole::PtraceNormalization { execution_ordinal },
                ),
            );
            accounted_cpu_usage_usec =
                checked_add_cpu_usage(accounted_cpu_usage_usec, normalized.cpu_usage_usec);
            normalization_error = normalized.result.err();
        }
    }
    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
    let mut outcome = if output.timeout.is_some() || !output.status.success() {
        "FAIL"
    } else {
        "PASS"
    }
    .to_string();
    let mut reason = output
        .timeout
        .map(|kind| kind.reason(cpu_timeout_seconds, wall_timeout_seconds));
    let mut error_kind = None;
    let failure_class_line = stderr.lines().next().unwrap_or_default();
    let launch_refusal = spec.id.mode != "naked"
        && !output.status.success()
        && stdout.is_empty()
        && matches!(
            failure_class_line,
            "HERMIT_INTERNAL_FAILURE class=guest-program-not-found"
                | "HERMIT_INTERNAL_FAILURE class=guest-program-not-executable"
        );
    if launch_refusal {
        outcome = "ERROR".into();
        error_kind = Some("guest-launch-refused".into());
        reason = Some(format!(
            "guest launch refused before execution: {}",
            stderr
                .lines()
                .find_map(|line| line.strip_prefix("Error: "))
                .unwrap_or("unknown launch refusal")
        ));
    }
    // A runner that cannot start the requested backend and a backend that ran
    // but produced no canonical comparison are different failures. Keep both
    // visible as ERROR, but give the pre-guest availability refusal its own
    // machine-readable kind so sweeps cannot count it as a product failure.
    // The first line is the producer-owned class emitted before human prose.
    // Matching a broader nonzero exit would hide real regressions.
    // KVM currently bypasses ensure_available, so its availability failures do
    // not enter this class.
    let unavailable_class = spec.id.backend.as_deref().map(|backend| {
        format!("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend={backend}")
    });
    let backend_unavailable = spec.id.mode != "naked"
        && !launch_refusal
        && output.timeout.is_none()
        && !output.status.success()
        && stdout.is_empty()
        && unavailable_class
            .as_deref()
            .is_some_and(|class| failure_class_line == class);
    if backend_unavailable {
        outcome = "ERROR".into();
        error_kind = Some("backend-unavailable".into());
        reason = Some(format!(
            "backend unavailable on this runner, so nothing was measured: {}",
            stderr
                .lines()
                .find_map(|line| line.strip_prefix("Error: "))
                .unwrap_or("unknown backend unavailability")
        ));
    }
    // A producer class that cannot satisfy the requested backend or execution
    // shape is unavailable evidence, not a product crash. In particular, the
    // human error line must never override a mismatched class into FAIL.
    let invalid_backend_evidence = output.timeout.is_none()
        && !output.status.success()
        && !backend_unavailable
        && failure_class_line
            .starts_with("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=");
    if invalid_backend_evidence {
        outcome = "ERROR".into();
        error_kind = Some("invalid-backend-evidence".into());
        reason = Some(format!(
            "backend availability evidence does not match this attempt: {failure_class_line}"
        ));
    }
    // The producer did write a class, but it did not establish a more specific
    // result. Keep that absence as no-result instead of letting the following
    // English line manufacture a product failure.
    let unclassified_internal_failure = output.timeout.is_none()
        && !output.status.success()
        && !launch_refusal
        && !backend_unavailable
        && !invalid_backend_evidence
        && failure_class_line == "HERMIT_INTERNAL_FAILURE class=cli-error";
    if unclassified_internal_failure {
        outcome = "ERROR".into();
        error_kind = Some("incomplete-verification-evidence".into());
        reason = stderr
            .lines()
            .find_map(|line| line.strip_prefix("Error: "))
            .filter(|detail| !detail.trim().is_empty())
            .map(str::to_owned);
    }
    let producer_failure_classified = launch_refusal
        || backend_unavailable
        || invalid_backend_evidence
        || unclassified_internal_failure;
    let mut report_json = None;
    let mut report_sha = None;
    let mut runtime = None;
    let mut first_divergent_scheduler_turn = None;
    let mut first_divergent_virtual_nanoseconds = None;
    let mut first_divergent_record = None;
    let mut first_divergent_syscall = None;
    let mut first_divergent_left_message = None;
    let mut first_divergent_right_message = None;
    let (sabre_path_evidence, sabre_path_evidence_sha256) = spec
        .sabre_path_evidence
        .as_ref()
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| {
            (
                Some(String::from_utf8_lossy(&bytes).into_owned()),
                Some(hex_digest(&bytes)),
            )
        })
        .unwrap_or((None, None));
    if let Some(path) = &spec.verdict_path {
        match fs::read(path) {
            Ok(bytes) => {
                report_sha = Some(hex_digest(&bytes));
                report_json = Some(String::from_utf8_lossy(&bytes).into_owned());
                match current_verification_report(&bytes) {
                    Ok(report) => {
                        runtime = report.runtime.clone();
                        // Recorded BEFORE the classification chain below,
                        // because where the divergence began is a fact about
                        // the report rather than a consequence of how the
                        // attempt is classified -- a FAIL and an ERROR can both
                        // carry a located divergence.
                        //
                        // Guarded on `launch_refusal` for the same reason the
                        // arm below refuses to reclassify: no guest was ever
                        // created, so any report present cannot describe an
                        // executed guest's divergence position.
                        if !launch_refusal && !backend_unavailable {
                            first_divergent_scheduler_turn = report.first_divergent_scheduler_turn;
                            first_divergent_virtual_nanoseconds =
                                report.first_divergent_virtual_nanoseconds;
                            first_divergent_record = report.first_divergent_record;
                            first_divergent_syscall = report.first_divergent_syscall;
                            first_divergent_left_message =
                                report.first_divergent_left_message.clone();
                            first_divergent_right_message =
                                report.first_divergent_right_message.clone();
                        }
                        if producer_failure_classified {
                            // The producer's classified failure, or a
                            // contradiction in that evidence, cannot be
                            // superseded by a pre-stamped or unrelated report.
                        } else if let Some(
                            crate::canonical_verdict::NoResultReason::ContainerFailed(failure),
                        ) = &report.no_result_reason
                        {
                            match failure.require_reporter_exit(
                                output.status.code(),
                                std::os::unix::process::ExitStatusExt::signal(&output.status),
                                output.timeout.is_some(),
                            ) {
                                Ok(()) => {
                                    outcome = "FAIL".into();
                                    error_kind = None;
                                    reason = Some(format!(
                                        "sandbox failed during {:?}: {:?}; no guest disposition or comparison was returned",
                                        failure.run, failure.disposition
                                    ));
                                }
                                Err(error) => {
                                    outcome = "ERROR".into();
                                    error_kind = Some("incomplete-verification-evidence".into());
                                    reason = Some(error);
                                }
                            }
                        } else if report.verdict == Verdict::InfrastructureError {
                            let comparison_error = report.comparison.as_ref().and_then(|_| {
                                match spec.comparator {
                                    Comparator::Strict => report.require_canonical_comparison(),
                                    Comparator::Stripped => require_stripped_comparison(&report),
                                }
                                .err()
                            });
                            if let Some(error) = comparison_error {
                                outcome = "ERROR".into();
                                error_kind = Some("incomplete-verification-evidence".into());
                                reason = Some(error);
                            } else {
                                outcome = "ERROR".into();
                                error_kind = Some("infrastructure".into());
                                reason = Some(match report.infrastructure_error.as_ref() {
                                    Some(InfrastructureError::SkidOvershoot { count }) => format!(
                                        "verification recorded {count} HERMIT_SKID_OVERSHOOT report(s)"
                                    ),
                                    None => unreachable!(
                                        "typed report parser requires an infrastructure error"
                                    ),
                                });
                            }
                        } else if report.verdict == Verdict::NoResult
                            && matches!(
                                report.no_result_reason,
                                Some(
                                    crate::canonical_verdict::NoResultReason::ComparisonRefused { .. }
                                )
                            )
                        {
                            let Some(crate::canonical_verdict::NoResultReason::ComparisonRefused {
                                detail,
                            }) = report.no_result_reason.as_ref()
                            else {
                                unreachable!()
                            };
                            outcome = "ERROR".into();
                            error_kind = Some("incomplete-verification-evidence".into());
                            reason = Some(detail.clone());
                        } else if report.verdict == Verdict::NoResult
                            && matches!(
                                report.no_result_reason,
                                Some(
                                    crate::canonical_verdict::NoResultReason::FirstRunRejected { .. }
                                ) | None
                            )
                            && output.timeout.is_none()
                            && output.status.code().is_some_and(|code| code != 0)
                        {
                            // The producer correctly has no comparison to
                            // report, but an ordinary nonzero process exit is
                            // still a completed failure rather than unknown
                            // infrastructure state.
                            outcome = "FAIL".into();
                            error_kind = None;
                            reason = Some(format!(
                                "{} exited with status {} before producing a terminal comparison",
                                spec.id.mode,
                                output.status.code().unwrap()
                            ));
                        } else if let Some(error) = (spec.comparator == Comparator::Stripped)
                            .then(|| require_stripped_comparison(&report).err())
                            .flatten()
                        {
                            outcome = "ERROR".into();
                            error_kind = Some("incomplete-verification-evidence".into());
                            reason = Some(error);
                        } else if spec.comparator == Comparator::Stripped
                            && !(report.verified && report.verdict == Verdict::Matched)
                        {
                            // Hermit's default --verify comparison, declared by
                            // the cell: it still requires a terminal matched
                            // verdict, but it is below L2 and never canonical.
                            outcome = "FAIL".into();
                            reason = Some(format!(
                                "stripped verification did not match: verified={} verdict={}",
                                report.verified, report.verdict
                            ));
                        } else if let Some(error) = (spec.comparator == Comparator::Stripped)
                            .then(|| report.require_exact_output_match().err())
                            .flatten()
                        {
                            // The two runs' exact outputs must agree as well;
                            // this does not depend on the log comparison.
                            outcome = "FAIL".into();
                            reason = Some(format!("stripped verification outputs: {error}"));
                        } else if let Some(error) = (spec.comparator == Comparator::Strict)
                            .then(|| report.require_canonical_comparison().err())
                            .flatten()
                        {
                            outcome = "ERROR".into();
                            error_kind = Some("incomplete-verification-evidence".into());
                            reason = Some(error);
                        } else if let Some(error) = (spec.comparator == Comparator::Strict)
                            .then(|| report.require_canonical_match().err())
                            .flatten()
                        {
                            outcome = "FAIL".into();
                            reason = Some(error);
                        } else if let Some(Err(error)) = spec
                            .expected_stdout
                            .as_deref()
                            .filter(|_| output.timeout.is_none())
                            .map(|expected| check_expected_stdout(expected, &report))
                        {
                            // Two runs agreeing on the wrong bytes is still a
                            // failure when the cell names the exact bytes.
                            match error {
                                ExpectedStdoutError::Unevidenced(error) => {
                                    outcome = "ERROR".into();
                                    error_kind =
                                        Some("incomplete-verification-evidence".into());
                                    reason = Some(error);
                                }
                                ExpectedStdoutError::Mismatch(error) => {
                                    outcome = "FAIL".into();
                                    reason = Some(error);
                                }
                            }
                        } else if let Some(Err(error)) = spec
                            .expected_stdout_contains
                            .as_deref()
                            .filter(|_| output.timeout.is_none())
                            .map(|text| match fs::read(&stdout_path) {
                                Ok(captured) => {
                                    check_expected_stdout_contains(text, &captured, &report)
                                }
                                Err(error) => Err(ExpectedStdoutError::Unevidenced(format!(
                                    "cannot read captured verify stdout {}: {error}, so the declared expected_stdout_contains cannot be checked",
                                    stdout_path.display()
                                ))),
                            })
                        {
                            // Agreeing runs that omit the guest's success
                            // marker are still a failure.
                            match error {
                                ExpectedStdoutError::Unevidenced(error) => {
                                    outcome = "ERROR".into();
                                    error_kind =
                                        Some("incomplete-verification-evidence".into());
                                    reason = Some(error);
                                }
                                ExpectedStdoutError::Mismatch(error) => {
                                    outcome = "FAIL".into();
                                    reason = Some(error);
                                }
                            }
                        } else if let Some(expected) = spec
                            .expected_guest_exit
                            .as_ref()
                            .filter(|_| output.timeout.is_none())
                        {
                            // A matched comparison of the wrong disposition is
                            // still a failure: the cell names one exact status.
                            match expected.check(
                                report.guest_exit_code,
                                report.guest_signal,
                                output.status.code(),
                                std::os::unix::process::ExitStatusExt::signal(&output.status),
                            ) {
                                Ok(()) => {
                                    outcome = "PASS".into();
                                    reason = None;
                                }
                                Err(error) => {
                                    outcome = "FAIL".into();
                                    reason = Some(error);
                                }
                            }
                        } else if output.timeout.is_none()
                            && (output.status.success() || spec.id.mode == "chaos")
                        {
                            // Chaos deliberately admits a reproduced nonzero
                            // guest class. Verify and replay still require the
                            // Hermit process itself to succeed unless a verify
                            // cell names its exact expected guest exit above,
                            // and no receipt may erase a timeout.
                            outcome = "PASS".into();
                            reason = None;
                        } else if reason.is_none() {
                            reason = Some(format!(
                                "{} exited with status {}",
                                spec.id.mode,
                                output.status.code().unwrap_or(128)
                            ));
                        }
                    }
                    Err(error) if !producer_failure_classified => {
                        outcome = "ERROR".into();
                        error_kind = Some("incomplete-verification-evidence".into());
                        reason = Some(format!("verification report is unreadable: {error}"));
                    }
                    Err(_) => {}
                }
            }
            Err(_error) if producer_failure_classified => {}
            Err(error) => {
                outcome = "ERROR".into();
                error_kind = Some("incomplete-verification-evidence".into());
                reason = Some(format!("verification report is missing: {error}"));
            }
        }
    }
    if let Some(timeout) = output.timeout {
        error_kind = Some(timeout.error_kind().into());
        reason = Some(timeout.reason(cpu_timeout_seconds, wall_timeout_seconds));
    }
    if let Some(error) = normalization_error {
        outcome = "ERROR".into();
        error_kind = Some("incomplete-verification-evidence".into());
        reason = Some(error);
    }
    Ok(AttemptResult {
        index: index.into(),
        outcome,
        error_kind,
        status: output.status.code(),
        signal: std::os::unix::process::ExitStatusExt::signal(&output.status),
        timed_out: output.timeout.is_some(),
        duration_ms: started.elapsed().as_millis(),
        cpu_usage_usec: accounted_cpu_usage_usec,
        observation_sha256: None,
        argv: spec.argv.clone(),
        guest_argv: spec.guest_argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.to_string_lossy().into_owned(),
        shell_command: shell_command(&spec.cwd.to_string_lossy(), &spec.env, &spec.argv),
        stdout,
        stderr,
        verification_report: report_json,
        verification_report_sha256: report_sha,
        runtime,
        first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds,
        first_divergent_record,
        first_divergent_syscall,
        first_divergent_left_message,
        first_divergent_right_message,
        sabre_path_evidence,
        sabre_path_evidence_sha256,
        reason,
    })
}

/// Preserve process accounting even when post-processing cannot produce its
/// semantic result. An `Err` must not erase CPU already consumed by a child.
struct AccountedStep<T> {
    result: Result<T, String>,
    cpu_usage_usec: Option<u64>,
}

fn normalize_ptrace_golden(
    hermit: &str,
    directory: &Path,
    cwd: &Path,
    deadline: Instant,
    remaining_cpu_usec: Option<u64>,
    timeout_seconds: (u64, u64),
    record: (&mut Vec<InvocationCpuObservation>, InvocationRole),
) -> AccountedStep<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            return AccountedStep {
                result: Err(format!("cannot read {}: {error}", directory.display())),
                cpu_usage_usec: Some(0),
            };
        }
    };
    let mut run1 = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .collect::<Vec<_>>();
    run1.sort();
    let normalized = directory.join("normalized-ptrace-golden.log");
    let status_path = directory.join("normalized-ptrace-golden.status");
    if run1.len() != 1 || !run1[0].metadata().is_ok_and(|metadata| metadata.len() > 0) {
        let result = fs::write(&normalized, b"")
            .and_then(|()| fs::write(status_path, b"2\n"))
            .map_err(|error| error.to_string());
        return AccountedStep {
            result,
            cpu_usage_usec: Some(0),
        };
    }
    let stderr = directory.join("normalized-ptrace-golden.stderr");
    let args = vec!["log-diff".into(), run1[0].to_string_lossy().into_owned()];
    let output = match execute_process(
        ProcessRequest::new(
            record.1,
            cwd,
            hermit,
            &args,
            &BTreeMap::new(),
            &normalized,
            &stderr,
        ),
        (deadline, remaining_cpu_usec),
        record.0,
    ) {
        Ok(output) => output,
        Err(error) => {
            return AccountedStep {
                result: Err(format!("cannot normalize {}: {error}", run1[0].display())),
                cpu_usage_usec: None,
            };
        }
    };
    let cpu_usage_usec = Some(output.cpu_usage_usec);
    let status = output.status.code().unwrap_or(1);
    let result = (|| {
        if !output.status.success() {
            fs::write(&normalized, b"").map_err(|error| error.to_string())?;
        }
        fs::write(&status_path, format!("{status}\n")).map_err(|error| error.to_string())?;
        if let Some(timeout) = output.timeout {
            return Err(format!(
                "ptrace golden normalization {}",
                timeout.reason(timeout_seconds.0, timeout_seconds.1)
            ));
        }
        Ok(())
    })();
    AccountedStep {
        result,
        cpu_usage_usec,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessTimeout {
    Wall,
    Cpu,
}

impl ProcessTimeout {
    fn error_kind(self) -> &'static str {
        match self {
            Self::Wall => "wall-timeout",
            Self::Cpu => "cpu-timeout",
        }
    }

    fn reason(self, cpu_seconds: u64, wall_seconds: u64) -> String {
        match self {
            Self::Wall => {
                format!("cell exceeded {wall_seconds} wall s backstop ({cpu_seconds} s CPU budget)")
            }
            Self::Cpu => format!("cell exceeded {cpu_seconds} CPU s"),
        }
    }
}

struct ProcessOutput {
    status: ExitStatus,
    timeout: Option<ProcessTimeout>,
    cpu_usage_usec: u64,
}

struct ProcessLimits {
    deadline: Instant,
    cpu_budget_usec: Option<u64>,
    cpu_poll_interval: Duration,
}

struct ExecutionBudget {
    deadline: Instant,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    remaining_cpu_usec: Option<u64>,
}

fn record_not_started(
    request: ProcessRequest,
    cpu_enabled: bool,
    reason: NotStartedReason,
    observations: &mut Vec<InvocationCpuObservation>,
) {
    let mut observation = InvocationCpuObservation::pending(
        observations.len() as u64 + 1,
        request.role,
        request.command,
        cpu_enabled,
    );
    observation.launch = LaunchObservation::NotStarted { reason };
    observations.push(observation);
}

/// Add two complete CPU measurements, refusing missing or overflowing input.
pub fn checked_add_cpu_usage(total: Option<u64>, usage: Option<u64>) -> Option<u64> {
    total.and_then(|total| usage.and_then(|usage| total.checked_add(usage)))
}

fn rusage_cpu_usage_usec(usage: &libc::rusage) -> Result<u64, String> {
    WaitCpuObservation::from_rusage(usage).total()
}

fn wait4_process(
    pid: u32,
    options: libc::c_int,
    operation: WaitOperation,
    started: Instant,
    final_wait: &mut FinalWaitObservation,
) -> Result<Option<(ExitStatus, u64)>, String> {
    loop {
        let mut status = 0;
        let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
        let waited = unsafe { libc::wait4(pid as libc::pid_t, &mut status, options, &mut usage) };
        if waited == 0 {
            return Ok(None);
        }
        if waited == pid as libc::pid_t {
            let cpu = WaitCpuObservation::from_rusage(&usage);
            let total = rusage_cpu_usage_usec(&usage);
            *final_wait = if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                FinalWaitObservation::Reaped {
                    source: FinalCpuSource::Wait4,
                    pid,
                    raw_status: status,
                    at: started.elapsed().into(),
                    cpu,
                }
            } else {
                FinalWaitObservation::NonterminalReturn {
                    source: FinalCpuSource::Wait4,
                    pid,
                    raw_status: status,
                    at: started.elapsed().into(),
                    cpu,
                }
            };
            return Ok(Some((ExitStatus::from_raw(status), total?)));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        let reason = format!("wait4({pid}) failed: {error}");
        *final_wait = FinalWaitObservation::Unavailable {
            operation,
            errno: error
                .raw_os_error()
                .map_or(RequiredNullable::Null, RequiredNullable::Value),
            reason: reason.clone(),
        };
        return Err(reason);
    }
}

fn live_cpu_usage_usec(pgid: u32, seconds: f64) -> Result<u64, String> {
    let usec = seconds * 1_000_000.0;
    // u64::MAX rounds up to 2^64 as f64, so that boundary is exclusive.
    if !usec.is_finite() || usec.is_sign_negative() || usec >= u64::MAX as f64 {
        return Err(format!(
            "process group {pgid} returned invalid live CPU seconds {seconds}"
        ));
    }
    Ok(usec as u64)
}

fn stop_process_group(
    pid: u32,
    started: Instant,
    final_wait: &mut FinalWaitObservation,
) -> Result<(ExitStatus, u64), String> {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    let grace = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = wait4_process(
            pid,
            libc::WNOHANG,
            WaitOperation::StopGrace,
            started,
            final_wait,
        )? {
            return Ok(result);
        }
        if Instant::now() >= grace {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    wait4_process(pid, 0, WaitOperation::BlockingStop, started, final_wait)?.ok_or_else(|| {
        let reason = format!("blocking wait4({pid}) returned no child");
        *final_wait = FinalWaitObservation::Unavailable {
            operation: WaitOperation::BlockingStop,
            errno: RequiredNullable::Null,
            reason: reason.clone(),
        };
        reason
    })
}

struct ProcessRequest {
    role: InvocationRole,
    command: CommandIdentity,
    cwd: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
}

impl ProcessRequest {
    fn new(
        role: InvocationRole,
        cwd: &Path,
        program: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        stdout: &Path,
        stderr: &Path,
    ) -> Self {
        Self {
            role,
            cwd: cwd.to_owned(),
            command: CommandIdentity {
                argv: std::iter::once(program.to_string())
                    .chain(args.iter().cloned())
                    .collect(),
                cwd: cwd.to_string_lossy().into_owned(),
                env_overrides: env.clone(),
            },
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }
}

fn execute_process(
    request: ProcessRequest,
    limits: (Instant, Option<u64>),
    observations: &mut Vec<InvocationCpuObservation>,
) -> Result<ProcessOutput, String> {
    execute_process_with_cpu_poll_interval(
        request,
        ProcessLimits {
            deadline: limits.0,
            cpu_budget_usec: limits.1,
            cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
        },
        observations,
    )
}

fn execute_process_with_cpu_poll_interval(
    request: ProcessRequest,
    limits: ProcessLimits,
    observations: &mut Vec<InvocationCpuObservation>,
) -> Result<ProcessOutput, String> {
    let started = Instant::now();
    let mut observation = InvocationCpuObservation::pending(
        observations.len() as u64 + 1,
        request.role,
        request.command,
        limits.cpu_budget_usec.is_some(),
    );
    let result = spawn_process(
        &request.cwd,
        &request.stdout,
        &request.stderr,
        &mut observation,
    )
    .and_then(|child| {
        monitor_process(
            child,
            limits,
            started,
            &mut observation,
            |pid| dagrun::proccpu::ProcessGroupCpu::new(pid).map_err(|error| error.to_string()),
            |reader| reader.seconds().map_err(|error| error.to_string()),
        )
    });
    // Retain the same observation on every error path, including successful
    // reap after accounting became unavailable. An error never fabricates zero.
    observations.push(observation);
    result
}

fn spawn_process(
    cwd: &Path,
    stdout: &Path,
    stderr: &Path,
    observation: &mut InvocationCpuObservation,
) -> Result<Child, String> {
    let mut open_capture = |path: &Path, stage| {
        File::create(path).map_err(|error| {
            let reason = format!("{}: {error}", path.display());
            observation.launch = LaunchObservation::SpawnFailed {
                stage,
                reason: reason.clone(),
            };
            observation.termination = TerminationPath::SpawnFailed;
            reason
        })
    };
    let stdout_file = open_capture(stdout, SpawnStage::StdoutCapture)?;
    let stderr_file = open_capture(stderr, SpawnStage::StderrCapture)?;
    let identity = &observation.command;
    let program = &identity.argv[0];
    let mut command = Command::new(program);
    command
        .args(&identity.argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);
    command.envs(identity.env_overrides.iter());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match command.spawn() {
        Ok(child) => {
            observation.launch = LaunchObservation::Spawned { pid: child.id() };
            Ok(child)
        }
        Err(error) => {
            let reason = format!("cannot execute {program}: {error}");
            observation.launch = LaunchObservation::SpawnFailed {
                stage: SpawnStage::Spawn,
                reason: reason.clone(),
            };
            observation.termination = TerminationPath::SpawnFailed;
            Err(reason)
        }
    }
}

fn monitor_process<R>(
    child: Child,
    limits: ProcessLimits,
    started: Instant,
    observation: &mut InvocationCpuObservation,
    register: impl FnOnce(u32) -> Result<R, String>,
    mut sample: impl FnMut(&R) -> Result<f64, String>,
) -> Result<ProcessOutput, String> {
    let ProcessLimits {
        deadline,
        cpu_budget_usec,
        cpu_poll_interval,
    } = limits;
    let pid = child.id();
    // Authenticate once at spawn and retain this generation's owner through
    // final wait/stop. Disabled accounting never registers or samples a reader.
    let cpu_reader = cpu_budget_usec.map(|_| register(pid));
    if let LiveCpuObservation::Enabled(live) = &mut observation.live {
        live.registration = match &cpu_reader {
            Some(Ok(_)) => RegistrationObservation::BoundOnce,
            Some(Err(reason)) => RegistrationObservation::Unavailable {
                reason: reason.clone(),
            },
            None => unreachable!("enabled CPU observation requires the existing CPU budget"),
        };
    }
    let mut next_cpu_poll = Instant::now() + cpu_poll_interval;
    let mut cpu_accounting_missing_since = None;
    loop {
        observation.termination = TerminationPath::WaitError;
        if let Some((status, cpu_usage_usec)) = wait4_process(
            pid,
            libc::WNOHANG,
            WaitOperation::Poll,
            started,
            &mut observation.final_wait,
        )? {
            let timeout = cpu_budget_usec
                .filter(|limit| cpu_usage_usec >= *limit)
                .map(|_| ProcessTimeout::Cpu);
            observation.termination = if timeout.is_some() {
                TerminationPath::FinalWaitCpuBudgetReturn
            } else if matches!(observation.final_wait, FinalWaitObservation::Reaped { .. }) {
                TerminationPath::CompletedWait4
            } else {
                TerminationPath::NonterminalWait4Return
            };
            observation.returned_cpu_charge = ReturnedCpuCharge::Value {
                cpu_usec: cpu_usage_usec,
                basis: ChargeBasis::FinalWait4,
            };
            return Ok(ProcessOutput {
                status,
                timeout,
                cpu_usage_usec,
            });
        }
        let now = Instant::now();
        let (timeout, observed_cpu_usec) = if now >= deadline {
            (Some(ProcessTimeout::Wall), None)
        } else if let Some(limit) = cpu_budget_usec.filter(|_| now >= next_cpu_poll) {
            next_cpu_poll = now + cpu_poll_interval;
            let sample_result = match &cpu_reader {
                Some(Ok(reader)) => sample(reader)
                    .map_err(|error| CpuError {
                        stage: CpuErrorStage::Sampling,
                        reason: format!("sampling: {error}"),
                    })
                    .and_then(|seconds| {
                        live_cpu_usage_usec(pid, seconds).map_err(|reason| CpuError {
                            stage: CpuErrorStage::Conversion,
                            reason,
                        })
                    }),
                Some(Err(error)) => Err(CpuError {
                    stage: CpuErrorStage::Registration,
                    reason: format!("registration: {error}"),
                }),
                None => unreachable!("CPU budget has no invocation reader"),
            };
            let triggered = sample_result.as_ref().is_ok_and(|used| *used >= limit);
            if let LiveCpuObservation::Enabled(live) = &mut observation.live {
                live.record_poll(
                    now.duration_since(started),
                    matches!(&cpu_reader, Some(Ok(_))),
                    &sample_result,
                    triggered,
                );
            }
            match sample_result {
                Ok(used) => {
                    cpu_accounting_missing_since = None;
                    (triggered.then_some(ProcessTimeout::Cpu), Some(used))
                }
                Err(error) => {
                    let reason = error.reason;
                    let missing_since = cpu_accounting_missing_since.get_or_insert(now);
                    if now.duration_since(*missing_since) >= CELL_CPU_ACCOUNTING_GRACE {
                        observation.termination = TerminationPath::AccountingUnavailableStop;
                        return match stop_process_group(pid, started, &mut observation.final_wait) {
                            Ok(_) => Err(format!(
                                "cannot measure live CPU for process group {pid}: {reason}; stopped and reaped its leader rather than silently disabling its CPU budget"
                            )),
                            Err(error) => Err(format!(
                                "cannot measure live CPU for process group {pid}: {reason}; could not confirm leader stop/reap: {error}"
                            )),
                        };
                    }
                    (None, None)
                }
            }
        } else {
            (None, None)
        };
        if let Some(timeout) = timeout {
            observation.termination = match timeout {
                ProcessTimeout::Cpu => TerminationPath::CpuBudgetStop,
                ProcessTimeout::Wall => TerminationPath::WallBudgetStop,
            };
            let (status, final_cpu_usage_usec) =
                stop_process_group(pid, started, &mut observation.final_wait)?;
            let cpu_usage_usec = observed_cpu_usec.map_or(final_cpu_usage_usec, |observed| {
                observed.max(final_cpu_usage_usec)
            });
            observation.returned_cpu_charge = ReturnedCpuCharge::Value {
                cpu_usec: cpu_usage_usec,
                basis: if observed_cpu_usec.is_some() {
                    ChargeBasis::MaxTriggerAndFinalWait4
                } else {
                    ChargeBasis::FinalWait4
                },
            };
            return Ok(ProcessOutput {
                status,
                timeout: Some(timeout),
                cpu_usage_usec,
            });
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn cell_timeout_attempt(
    spec: &CellRunSpec,
    cpu_timeout_seconds: u64,
    wall_timeout_seconds: u64,
    duration: Duration,
    timeout: ProcessTimeout,
) -> AttemptResult {
    let index = spec.attempt.as_str();
    // A post-preparation bound was already exhausted before this attempt could
    // start, so no process exists from which to recover an exit status or signal.
    // Record that fact with the
    // same typed NotRun report Hermit uses before its first guest run.  A
    // synthetic exit code would make a scheduler decision look like a process
    // result, while a bare FAIL makes the retained attempt unreadable to every
    // typed consumer.
    let report = serde_json::to_string(&VerificationReport::no_result())
        .expect("the typed NotRun report must serialize");
    let report_sha256 = hex_digest(report.as_bytes());
    AttemptResult {
        index: index.into(),
        outcome: "ERROR".into(),
        error_kind: Some(timeout.error_kind().into()),
        status: None,
        signal: None,
        timed_out: true,
        duration_ms: duration.as_millis(),
        cpu_usage_usec: None,
        observation_sha256: None,
        argv: spec.argv.clone(),
        guest_argv: spec.guest_argv.clone(),
        env: spec.env.clone(),
        cwd: spec.cwd.to_string_lossy().into_owned(),
        shell_command: shell_command(&spec.cwd.to_string_lossy(), &spec.env, &spec.argv),
        stdout: String::new(),
        stderr: String::new(),
        verification_report: Some(report),
        verification_report_sha256: Some(report_sha256),
        runtime: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        sabre_path_evidence: None,
        sabre_path_evidence_sha256: None,
        reason: Some(format!(
            "{} before attempt {index} started",
            timeout.reason(cpu_timeout_seconds, wall_timeout_seconds)
        )),
    }
}

/// The one definition of where a cell's retained evidence lives.
///
/// Was duplicated verbatim in `run_cell` and `infrastructure_error_result`.
/// Two copies of a path formula is how a log line ends up pointing at a
/// directory that does not exist, so surfacing the path made collapsing them a
/// prerequisite rather than a tidy-up.
fn cell_artifact_dir(context: &RunContext, cell: &SelectedCell) -> PathBuf {
    cell_artifact_path(
        &context.result_root,
        &context.run_id,
        &cell.id,
        context.attempt,
    )
}

/// Construct the producer-owned path for one cell's outer attempt.
/// Readers use this same definition to authenticate a retained fixture's
/// relocation; they must still validate the recorded root and invocation.
pub fn cell_artifact_path(
    result_root: &Path,
    run_id: &str,
    cell: &CellId,
    attempt: u64,
) -> PathBuf {
    let base_slug = format!(
        "{}-{}-{}",
        cell.test.replace('/', "-"),
        cell.mode,
        cell.backend.as_deref().unwrap_or("none")
    );
    let slug = if attempt == 1 {
        base_slug
    } else {
        format!("{base_slug}-attempt-{attempt}")
    };
    result_root.join("runs").join(run_id).join(slug)
}

fn verification_verdict(attempt: &AttemptResult) -> Option<Verdict> {
    let report = attempt.verification_report.as_deref()?;
    current_verification_report(report.as_bytes())
        .ok()
        .map(|report| report.verdict)
}

fn observed_result(
    mode: &str,
    outcome: &str,
    attempts: &[AttemptResult],
    error_kind: Option<&str>,
) -> Option<ObservedResult> {
    // Preserve the existing terminal evidence and timeout guards before using
    // a typed product verdict. Incidental diagnostic text cannot erase a valid
    // comparison, but an earlier divergence cannot revive an unusable terminal
    // framework/evidence result either.
    let typed = observed_result_from_typed_evidence(mode, outcome, attempts, error_kind);
    if matches!(
        typed,
        Some(
            ObservedResult::Pass
                | ObservedResult::DeterminismFailure
                | ObservedResult::ReplayFailure
        )
    ) {
        return typed;
    }
    if let Some(class) = attempts.iter().find_map(|attempt| {
        let output = format!("{}\n{}", attempt.stdout, attempt.stderr);
        environmental_block_observation(&output).block_class()
    }) {
        return Some(if class == EnvBlockClass::BpfjailerBanner {
            ObservedResult::SandboxDenied
        } else {
            ObservedResult::InfrastructureError
        });
    }
    typed
}

fn observed_result_from_typed_evidence(
    mode: &str,
    outcome: &str,
    attempts: &[AttemptResult],
    error_kind: Option<&str>,
) -> Option<ObservedResult> {
    if outcome == "PASS" {
        return Some(ObservedResult::Pass);
    }
    // A later framework or evidence failure decides whether this cell produced
    // a usable product result. An earlier attempt can still retain a located
    // divergence in `attempts`, but it must not make a terminal infrastructure
    // or no-result outcome look product-attributed.
    if non_product_failure_class(error_kind).is_some() {
        return None;
    }
    if attempts.iter().any(|attempt| attempt.timed_out) {
        return Some(ObservedResult::Timeout);
    }
    if mode == "verify"
        && attempts
            .iter()
            .any(|attempt| verification_verdict(attempt) == Some(Verdict::Diverged))
    {
        return Some(ObservedResult::DeterminismFailure);
    }
    if mode == "replay"
        && attempts
            .iter()
            .any(|attempt| verification_verdict(attempt) == Some(Verdict::Diverged))
    {
        return Some(ObservedResult::ReplayFailure);
    }
    (outcome == "FAIL").then_some(ObservedResult::CrashError)
}

fn non_product_failure_class(error_kind: Option<&str>) -> Option<FailureClass> {
    match error_kind {
        Some("guest-launch-refused" | "backend-unavailable") => {
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        }
        Some("infrastructure" | "result-publication") => {
            Some(FailureClass::UnderstoodInfrastructureFailure)
        }
        Some("incomplete-verification-evidence" | "invalid-backend-evidence") => {
            Some(FailureClass::NoResult)
        }
        _ => None,
    }
}

fn failure_class(
    outcome: &str,
    result: Option<ObservedResult>,
    error_kind: Option<&str>,
) -> Option<FailureClass> {
    if outcome == "PASS" {
        return None;
    }
    if matches!(
        result,
        Some(ObservedResult::SandboxDenied | ObservedResult::InfrastructureError)
    ) {
        return Some(FailureClass::UnderstoodInfrastructureFailure);
    }
    if let Some(failure_class) = non_product_failure_class(error_kind) {
        return Some(failure_class);
    }
    if let Some(result) = result {
        if let Some(failure_class) = result.failure_class() {
            return Some(failure_class);
        }
    }
    if outcome == "HOST-INAPPLICABLE" {
        Some(FailureClass::UnderstoodPrerequisiteFailure)
    } else {
        Some(FailureClass::NoResult)
    }
}

/// Apply a failed chaos population assertion without erasing an attempt-level
/// infrastructure result. A timeout or unavailable verification report means
/// the population was not fully measured; it is not a product failure merely
/// because the incomplete sample also missed `min_passes`. A timed-out
/// attempt's outcome is FAIL, not ERROR, so its cause arrives as `incomplete`
/// ([`chaos_incomplete_reason`]) and leads the reason.
fn apply_failed_chaos_assertion(
    outcome: &mut String,
    reason: &mut Option<String>,
    incomplete: Option<&str>,
    assertion_reason: String,
) {
    if outcome != "ERROR" {
        *outcome = "FAIL".into();
        *reason = Some(match incomplete {
            Some(incomplete) => format!("{incomplete}; {assertion_reason}"),
            None => assertion_reason,
        });
    }
}

/// A chaos population's passes, failures and timed-out attempts. A timed-out
/// attempt is neither a pass nor a failure, even when the guest exited 0 just
/// as the cell's CPU budget ran out: counting it as a pass would claim a seed
/// the budget refused, and counting a killed seed as a failure would satisfy
/// `min_failures` with no product failure.
fn chaos_population_counts(attempts: &[AttemptResult]) -> (u64, u64, u64) {
    let timed_out = attempts.iter().filter(|attempt| attempt.timed_out).count() as u64;
    let passes = attempts
        .iter()
        .filter(|attempt| attempt.status == Some(0) && !attempt.timed_out)
        .count() as u64;
    (
        passes,
        attempts.len() as u64 - passes - timed_out,
        timed_out,
    )
}

/// Why a chaos population is incomplete: the attempt that timed out, which
/// stops the seed loop, and how many planned seeds never ran. An earlier
/// seed's reason (a product failure before the budget ran out) follows it,
/// naming that seed, so the timeout does not hide it.
fn chaos_incomplete_reason(attempts: &[AttemptResult], planned_seeds: usize) -> Option<String> {
    let timed_out = attempts.iter().find(|attempt| attempt.timed_out)?;
    let incomplete = format!(
        "{}: {} at {}; {} of {planned_seeds} seeds ran",
        timed_out.error_kind.as_deref().unwrap_or("timeout"),
        timed_out
            .reason
            .as_deref()
            .unwrap_or("the attempt timed out"),
        timed_out.index,
        attempts.len(),
    );
    let earlier = attempts
        .iter()
        .take_while(|attempt| !attempt.timed_out)
        .find_map(|attempt| Some((&attempt.index, attempt.reason.as_deref()?)));
    Some(match earlier {
        Some((index, reason)) => format!("{incomplete}; {index}: {reason}"),
        None => incomplete,
    })
}

/// A chaos cell's reason before its population floors are checked: a
/// timeout's incomplete reason leads, unless an attempt-level ERROR already
/// explains the cell.
fn chaos_cell_reason(
    outcome: &str,
    reason: Option<String>,
    incomplete: Option<&str>,
) -> Option<String> {
    match incomplete {
        Some(incomplete) if outcome != "ERROR" => Some(incomplete.to_string()),
        _ => reason,
    }
}

#[derive(Default)]
struct CellRunProgress {
    observations: Vec<InvocationCpuObservation>,
    attempts: Vec<AttemptResult>,
}

/// A failed cell still owns every invocation and completed semantic attempt.
/// The harness consumes this once when it publishes the infrastructure row.
#[derive(Debug)]
pub struct CellRunFailure {
    reason: String,
    observations: Box<CellCpuObservationsV1>,
    attempts: Vec<AttemptResult>,
}

impl std::fmt::Display for CellRunFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.reason.fmt(f)
    }
}

impl std::error::Error for CellRunFailure {}

impl CellRunFailure {
    pub fn into_result(self, context: &RunContext, cell: &SelectedCell) -> CellResult {
        let mut result = infrastructure_error_result(context, cell, self.reason);
        result.cpu_observations = Some(*self.observations);
        result.attempts = self.attempts;
        result
    }
}

fn empty_cpu_observations(context: &RunContext, cell: &SelectedCell) -> CellCpuObservationsV1 {
    CellCpuObservationsV1 {
        version: 1,
        binding: CellCpuBinding {
            run_id: context.run_id.clone(),
            hermit_sha: context.source_sha.clone(),
            lane: cell.test.lane.clone(),
            category: cell.category.clone(),
            test: cell.id.test.clone(),
            mode: cell.id.mode.clone(),
            backend: cell
                .id
                .backend
                .clone()
                .map_or(RequiredNullable::Null, RequiredNullable::Value),
            outer_attempt: context.attempt,
            run_index: context
                .run_index
                .map_or(RequiredNullable::Null, RequiredNullable::Value),
        },
        invocations: Vec::new(),
    }
}

fn record_cell(
    context: &RunContext,
    cell: &SelectedCell,
    run: impl FnOnce(&mut CellRunProgress) -> Result<CellResult, String>,
) -> Result<CellResult, CellRunFailure> {
    let mut progress = CellRunProgress::default();
    let outcome = run(&mut progress);
    let mut observations = empty_cpu_observations(context, cell);
    observations.invocations = progress.observations;
    match outcome {
        Ok(mut result) => {
            result.cpu_observations = Some(observations);
            if let Err(reason) = result.require_cpu_observations() {
                return Err(CellRunFailure {
                    reason: format!("invalid generated CPU observations: {reason}"),
                    observations: Box::new(result.cpu_observations.take().expect("attached above")),
                    attempts: result.attempts,
                });
            }
            Ok(result)
        }
        Err(reason) => Err(CellRunFailure {
            reason,
            observations: Box::new(observations),
            attempts: progress.attempts,
        }),
    }
}

pub fn run_cell(context: &RunContext, cell: &SelectedCell) -> Result<CellResult, CellRunFailure> {
    run_cell_in(context, cell, cell_artifact_dir(context, cell))
}

/// Run `cell` with `dir` as its cell directory. Only [`run_cell`] and the
/// equal-inputs acceptance test choose a directory other than the cell's own.
fn run_cell_in(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
) -> Result<CellResult, CellRunFailure> {
    record_cell(context, cell, |progress| {
        run_cell_inner(context, cell, dir, progress)
    })
}

// All modes/backends using this fixture share the existing host inode with
// integration-test OFD producers. Other cells do not acquire a lease.
fn acquire_proc_locks_lease(
    cell: &SelectedCell,
    deadline: Instant,
    runtime_directory: Option<&OsStr>,
    host_identity: Option<&OsStr>,
) -> Result<Option<File>, String> {
    if cell.test.program.as_deref() != Some("tests/c/proc_locks.c") {
        return Ok(None);
    }
    use crate::proc_locks_lease;
    let uid = proc_locks_lease::effective_uid();
    let fallback = PathBuf::from(format!("/run/user/{uid}"));
    let acquire = || -> std::io::Result<File> {
        let directory =
            proc_locks_lease::validated_runtime_directory(runtime_directory, &fallback, uid)?;
        let file = proc_locks_lease::open_proc_locks_snapshot_lease_file(&directory, uid)?;
        proc_locks_lease::validate_proc_locks_host_identity(&file, host_identity)?;
        proc_locks_lease::lock_until(file, deadline)
    };
    acquire()
        .map(Some)
        .map_err(|error| format!("proc-locks snapshot lease: {error}"))
}

fn run_cell_inner(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    progress: &mut CellRunProgress,
) -> Result<CellResult, String> {
    let CellRunProgress {
        observations,
        attempts,
    } = progress;
    let started = Instant::now();
    let timeouts = cell_timeouts(context, cell)?;
    let preparation_deadline =
        execution_deadline_after_preparation(started, timeouts.wall_seconds)?;
    let binary_before = file_digest(&context.hermit_bin);
    let (guest, preparation_cpu_usage_usec) = prepare_test_until(
        context,
        cell,
        &dir,
        preparation_deadline,
        timeouts.wall_seconds,
        observations,
    )?;
    // Fixture preparation keeps its scaled wall-clock guard above. Post-preparation
    // execution uses its separately measured bound as an aggregate CPU-second budget
    // across all attempts/seeds, so time spent descheduled by a busy host cannot
    // turn a valid run into no_result. The wider wall deadline is retained only
    // for a wedged process that consumes no CPU and therefore cannot reach the
    // primary bound.
    let deadline = execution_deadline_after_preparation(Instant::now(), timeouts.wall_seconds)?;
    // Keep the guard through every repeat. Waiting consumes
    // this same deadline; acquisition failure returns through record_cell before
    // any guest attempt can be created, preserving an infrastructure no-result.
    let _proc_locks_lease = acquire_proc_locks_lease(
        cell,
        deadline,
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
        std::env::var_os("HERMIT_PROC_LOCKS_LEASE_ID").as_deref(),
    )?;
    let execution_cpu_budget_usec = timeouts.cpu_seconds.saturating_mul(1_000_000);
    let mut execution_cpu_usage_usec = 0u64;
    let mode = cell.test.modes.get(&cell.id.mode).unwrap();
    match cell.id.mode.as_str() {
        "naked" => {
            for index in 1..=mode.runs.unwrap_or(3) {
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index.to_string(),
                    None,
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    ExecutionBudget {
                        deadline,
                        cpu_timeout_seconds: timeouts.cpu_seconds,
                        wall_timeout_seconds: timeouts.wall_seconds,

                        remaining_cpu_usec: Some(
                            execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec),
                        ),
                    },
                    observations,
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        "chaos" => {
            let seeds = mode
                .seeds
                .as_ref()
                .ok_or_else(|| format!("{} chaos has no seeds", cell.id.test))?;
            if seeds.is_empty() {
                return Err(format!("{} chaos has no seeds", cell.id.test));
            }
            for seed in seeds {
                let index = format!("seed-{seed}");
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index,
                    Some(*seed),
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    ExecutionBudget {
                        deadline,
                        cpu_timeout_seconds: timeouts.cpu_seconds,
                        wall_timeout_seconds: timeouts.wall_seconds,

                        remaining_cpu_usec: Some(
                            execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec),
                        ),
                    },
                    observations,
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        "custom" => {
            for index in 1..=mode.assert.as_ref().and_then(|a| a.runs).unwrap_or(1) {
                let spec = build_spec(
                    context,
                    cell,
                    dir.clone(),
                    guest.clone(),
                    &index.to_string(),
                    None,
                    remaining_cell_seconds(deadline),
                )?;
                attempts.push(execute_observed_until(
                    &spec,
                    &cell.test.observation,
                    &dir,
                    ExecutionBudget {
                        deadline,
                        cpu_timeout_seconds: timeouts.cpu_seconds,
                        wall_timeout_seconds: timeouts.wall_seconds,

                        remaining_cpu_usec: Some(
                            execution_cpu_budget_usec.saturating_sub(execution_cpu_usage_usec),
                        ),
                    },
                    observations,
                )?);
                execution_cpu_usage_usec = execution_cpu_usage_usec
                    .checked_add(
                        attempts
                            .last()
                            .and_then(|attempt| attempt.cpu_usage_usec)
                            .unwrap_or(0),
                    )
                    .ok_or_else(|| "cell execution CPU usage overflowed u64".to_string())?;
                if attempts.last().is_some_and(|attempt| attempt.timed_out) {
                    break;
                }
            }
        }
        _ => {
            let spec = build_spec(
                context,
                cell,
                dir.clone(),
                guest.clone(),
                "1",
                None,
                remaining_cell_seconds(deadline),
            )?;
            attempts.push(execute_observed_until(
                &spec,
                &cell.test.observation,
                &dir,
                ExecutionBudget {
                    deadline,
                    cpu_timeout_seconds: timeouts.cpu_seconds,
                    wall_timeout_seconds: timeouts.wall_seconds,

                    remaining_cpu_usec: Some(execution_cpu_budget_usec),
                },
                observations,
            )?);
        }
    }
    let hashes = attempts
        .iter()
        .filter_map(|attempt| attempt.observation_sha256.clone())
        .collect::<Vec<_>>();
    let distinct = hashes.iter().collect::<BTreeSet<_>>().len() as u64;
    let mut outcome = if attempts.iter().all(|attempt| attempt.outcome == "PASS") {
        "PASS"
    } else if attempts.iter().any(|attempt| attempt.outcome == "ERROR") {
        "ERROR"
    } else {
        "FAIL"
    }
    .to_string();
    let mut reason = attempts.iter().find_map(|attempt| attempt.reason.clone());
    let position = cell_divergence_position(attempts);
    let first_divergent_scheduler_turn = position.scheduler_turn;
    let first_divergent_virtual_nanoseconds = position.virtual_nanoseconds;
    let first_divergent_record = position.record;
    let first_divergent_syscall = position.syscall;
    let first_divergent_left_message = position.left_message;
    let first_divergent_right_message = position.right_message;
    let mut error_kind = attempts
        .iter()
        .find_map(|attempt| attempt.error_kind.clone());
    if cell.id.mode == "naked" {
        let minimum = mode
            .assert
            .as_ref()
            .and_then(|a| a.min_distinct)
            .unwrap_or(2);
        if distinct < minimum {
            outcome = "FAIL".into();
            reason = Some(format!(
                "naked observed {distinct} distinct outcomes, need {minimum}"
            ));
        }
    }
    if cell.id.mode == "custom"
        && mode.assert.as_ref().and_then(|a| a.repeat_identical) == Some(true)
        && distinct != 1
    {
        outcome = "FAIL".into();
        reason = Some(format!("custom observed {distinct} distinct outcomes"));
    }
    if cell.id.mode == "chaos" {
        let assert = mode.assert.as_ref().cloned().unwrap_or_default();
        let (pass_count, failure_count, timed_out_count) = chaos_population_counts(attempts);
        let incomplete = chaos_incomplete_reason(attempts, mode.seeds.as_ref().map_or(0, Vec::len));
        reason = chaos_cell_reason(&outcome, reason, incomplete.as_deref());
        let diversity = diversity_evidence(
            &hashes,
            mode.outcome_classes,
            assert.min_distinct.unwrap_or(2),
            assert.min_normalized_entropy,
        );
        let normalized_entropy = diversity["normalized_entropy"].as_f64().unwrap_or(0.0);
        // ⚠️ THESE ARE FLOORS, AND A FLOOR OF ZERO IS NOT A CHECK.
        //
        // `min_passes` and `min_failures` are both unsigned, so a declared value
        // of 0 can never fire: `x < 0` is unsatisfiable. In a manifest
        // `min_failures: 0` READS like a threshold and asserts nothing, which is
        // the shape that let a required cell stay green while most of its seeds
        // exited nonzero. Five cells currently declare it; only
        // `chaos-c/lock-granularity` and one determinism-stress cell declare a
        // real value, and for those the floor is the point -- they are race
        // DEMONSTRATORS whose nonzero exits are the intended signal, so a run
        // with no failures means the race stopped being exposed.
        //
        // The matching trap on the other side is a `min_passes` of 1 against a
        // large seed list: it constrains exit status only to "at least one seed
        // exited 0", so every other seed may fail. Set `min_passes` to the seed
        // count unless failures are part of what the cell exists to observe.
        //
        // Neither floor bounds the population from ABOVE. Nothing here refuses a
        // cell whose failures grow, only one whose failures vanish.
        if distinct < assert.min_distinct.unwrap_or(2)
            || pass_count < assert.min_passes.unwrap_or(0)
            || failure_count < assert.min_failures.unwrap_or(0)
            || assert
                .min_normalized_entropy
                .is_some_and(|minimum| normalized_entropy < minimum)
        {
            apply_failed_chaos_assertion(
                &mut outcome,
                &mut reason,
                incomplete.as_deref(),
                format!(
                    "chaos distinct={distinct} passes={pass_count} failures={failure_count} timed_out={timed_out_count} normalized_entropy={normalized_entropy:.4}"
                ),
            );
        }
    }
    let execution_path = match summarize_sabre_path_evidence(attempts) {
        Ok(value) => value,
        Err(error) => {
            outcome = "ERROR".into();
            error_kind = Some("invalid-backend-evidence".into());
            reason = Some(error);
            None
        }
    };
    if outcome == "PASS"
        && execution_path
            .as_ref()
            .is_some_and(|evidence| evidence["eligible"] != true)
    {
        outcome = "FAIL".into();
        reason = Some("SaBRe execution path is incomplete or used fallback/native sites".into());
    }
    let literal_argv = attempts
        .first()
        .map(|attempt| attempt.argv.clone())
        .unwrap_or_default();
    let literal_guest_argv = attempts
        .first()
        .map(|attempt| attempt.guest_argv.clone())
        .unwrap_or_else(|| guest.clone());
    let literal_env = attempts
        .first()
        .map(|attempt| attempt.env.clone())
        .unwrap_or_else(|| execution_cell_env(context, &dir, cell.id.mode != "naked"));
    let literal_cwd = attempts
        .first()
        .map(|attempt| attempt.cwd.clone())
        .unwrap_or_else(|| context.root.to_string_lossy().into_owned());
    let literal_shell_command = attempts
        .first()
        .map(|attempt| attempt.shell_command.clone())
        .unwrap_or_default();
    let test_sha = test_digest(&context.root, &cell.test)?;
    let binary_sha = file_digest(&context.hermit_bin);
    if binary_before.is_some() && binary_before != binary_sha {
        outcome = "ERROR".into();
        error_kind = Some("infrastructure".into());
        reason = Some("Hermit binary changed while the cell was executing".into());
    }
    let result = observed_result(&cell.id.mode, &outcome, attempts, error_kind.as_deref());
    let failure_class = failure_class(&outcome, result, error_kind.as_deref());
    let cpu_usage_usec = attempts
        .iter()
        .try_fold(preparation_cpu_usage_usec, |total, attempt| {
            checked_add_cpu_usage(Some(total), attempt.cpu_usage_usec)
        });
    Ok(CellResult {
        cpu_observations: None,
        artifact_dir: dir.display().to_string(),
        expected_guest_exit: cell_expected_guest_exit(cell),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: binary_sha,
        test_sha256: test_sha,
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        // `required` records that the selected backend is an executable cell.
        // `ci` controls ordinary validate selection, not whether a manual red
        // measurement is real. Pressure re-runs enabled ci=false cells and
        // must be able to admit their evidence under the same identity.
        classification: cell_classification(cell).into(),
        outcome,
        result,
        failure_class,
        error_kind,
        timeout_seconds: timeouts.wall_seconds,
        execution_cpu_timeout_seconds: Some(timeouts.cpu_seconds),
        execution_wall_timeout_seconds: Some(timeouts.wall_seconds),
        duration_ms: Some(started.elapsed().as_millis()),
        cpu_usage_usec,
        runtime: attempts.iter().find_map(|attempt| attempt.runtime.clone()),
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: literal_argv.iter().skip(1).cloned().collect(),
        argv: literal_argv,
        guest_argv: literal_guest_argv,
        env: literal_env,
        cwd: literal_cwd,
        shell_command: literal_shell_command,
        relaxations: cell_relaxations(cell),
        execution_path,
        diversity: (cell.id.mode == "chaos").then(|| {
            diversity_evidence(
                &hashes,
                mode.outcome_classes,
                mode.assert
                    .as_ref()
                    .and_then(|assert| assert.min_distinct)
                    .unwrap_or(2),
                mode.assert
                    .as_ref()
                    .and_then(|assert| assert.min_normalized_entropy),
            )
        }),
        attempts: std::mem::take(attempts),
        backend_parity: None,
        first_divergent_scheduler_turn,
        first_divergent_virtual_nanoseconds,
        first_divergent_record,
        first_divergent_syscall,
        first_divergent_left_message,
        first_divergent_right_message,
        reason,
    })
}

/// Publish a durable third-outcome row when a cell could not reach execution.
///
/// Infrastructure failures are neither product failures nor admissible green
/// evidence.  Keeping an explicit row prevents preparation/spawn failures from
/// silently disappearing while leaving literal argv empty rather than
/// inventing a command that was never executed.
pub fn infrastructure_error_result(
    context: &RunContext,
    cell: &SelectedCell,
    reason: String,
) -> CellResult {
    let dir = cell_artifact_dir(context, cell);
    let timeouts = cell_timeouts(context, cell).ok();
    CellResult {
        cpu_observations: Some(empty_cpu_observations(context, cell)),
        artifact_dir: dir.display().to_string(),
        expected_guest_exit: cell_expected_guest_exit(cell),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: file_digest(&context.hermit_bin),
        test_sha256: test_digest(&context.root, &cell.test).unwrap_or_default(),
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        classification: cell_classification(cell).into(),
        outcome: "ERROR".into(),
        result: None,
        failure_class: Some(FailureClass::UnderstoodInfrastructureFailure),
        error_kind: Some("infrastructure".into()),
        timeout_seconds: timeouts
            .map(|policy| policy.wall_seconds)
            .unwrap_or(cell.timeout_seconds),
        execution_cpu_timeout_seconds: timeouts.map(|policy| policy.cpu_seconds),
        execution_wall_timeout_seconds: timeouts.map(|policy| policy.wall_seconds),
        duration_ms: None,
        cpu_usage_usec: None,
        runtime: None,
        log_level: (cell.id.mode != "naked").then(|| "info".into()),
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: execution_cell_env(context, &dir, cell.id.mode != "naked"),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: cell_relaxations(cell),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        backend_parity: None,
        // No attempt ran, so there is no divergence position to report. `None`
        // here means "never measured", which is the same value a clean run
        // produces -- see the note in the observation fold about those two
        // being indistinguishable in this field alone. The surrounding row is
        // what distinguishes them: this one carries an infrastructure error
        // kind and an empty `attempts`.
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        reason: Some(reason),
    }
}

/// Publish a typed result for a cell that provably cannot execute on this host.
///
/// No argv, environment, binary identity, or attempt is invented: the cell did
/// not run. Its normal identity and source digest remain present so the row
/// stays in the selected denominator and downstream completeness checks can
/// distinguish explicit inapplicability from missing output.
pub fn host_inapplicable_result(
    context: &RunContext,
    cell: &SelectedCell,
    reason: String,
) -> CellResult {
    let dir = cell_artifact_dir(context, cell);
    let timeouts = cell_timeouts(context, cell).ok();
    CellResult {
        cpu_observations: Some(empty_cpu_observations(context, cell)),
        artifact_dir: dir.display().to_string(),
        expected_guest_exit: cell_expected_guest_exit(cell),
        schema: CELL_RESULT_SCHEMA,
        run_id: context.run_id.clone(),
        machine_shortname: context.machine_shortname.clone(),
        kernel_version: context.kernel_version.clone(),
        host_capabilities: context.host_capabilities.clone(),
        attempt: context.attempt,
        run_index: context.run_index,
        hermit_sha: context.source_sha.clone(),
        binary_build_sha: context.binary_build_sha.clone(),
        source_tree_dirty: context.source_dirty,
        binary_sha256: None,
        test_sha256: test_digest(&context.root, &cell.test).unwrap_or_default(),
        test: cell.id.test.clone(),
        category: cell.category.clone(),
        lane: cell.test.lane.clone(),
        mode: cell.id.mode.clone(),
        backend: cell.id.backend.clone(),
        classification: cell_classification(cell).into(),
        outcome: "HOST-INAPPLICABLE".into(),
        result: None,
        failure_class: Some(FailureClass::UnderstoodPrerequisiteFailure),
        error_kind: None,
        timeout_seconds: timeouts
            .map(|policy| policy.wall_seconds)
            .unwrap_or(cell.timeout_seconds),
        execution_cpu_timeout_seconds: timeouts.map(|policy| policy.cpu_seconds),
        execution_wall_timeout_seconds: timeouts.map(|policy| policy.wall_seconds),
        duration_ms: None,
        cpu_usage_usec: None,
        runtime: None,
        log_level: None,
        effective_args: Vec::new(),
        argv: Vec::new(),
        guest_argv: Vec::new(),
        env: BTreeMap::new(),
        cwd: context.root.to_string_lossy().into_owned(),
        shell_command: String::new(),
        relaxations: cell_relaxations(cell),
        execution_path: None,
        diversity: None,
        attempts: Vec::new(),
        backend_parity: None,
        first_divergent_scheduler_turn: None,
        first_divergent_virtual_nanoseconds: None,
        first_divergent_record: None,
        first_divergent_syscall: None,
        first_divergent_left_message: None,
        first_divergent_right_message: None,
        reason: Some(reason),
    }
}

/// Reduce per-attempt divergence evidence to the cell-level fields.
///
/// Same first-attempt rule as `reason`, and resolved PER COORDINATE rather than
/// per attempt: a report that located a turn but no virtual nanosecond still
/// contributes the turn. Picking one attempt and taking both of its values
/// would silently drop the other coordinate.
///
/// Deliberately NOT a min across attempts. A min over the attempts of one run
/// is a one-run aggregate that would read like the cross-run `ObservedRange`
/// earliest/latest in ci/compat-envelope/scorecard.rs without having a sample
/// behind it, and this project has repeatedly been bitten by numbers that look
/// like a measurement and are not.
/// The divergence coordinates and compared messages of one cell.
///
/// A named struct rather than a tuple, deliberately: the four numeric values
/// are otherwise indistinguishable, and this project has already had a bare
/// ordinal read against the wrong axis. Each numeric field is a different
/// keyspace -- measured on one real divergence, the same event was record 98,
/// syscall 37 and scheduler turn 4. The messages retain the compared event
/// content without treating record position as identity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DivergencePosition {
    pub scheduler_turn: Option<u64>,
    pub virtual_nanoseconds: Option<u64>,
    pub record: Option<u64>,
    pub syscall: Option<u64>,
    pub left_message: Option<String>,
    pub right_message: Option<String>,
}

fn cell_divergence_position(attempts: &[AttemptResult]) -> DivergencePosition {
    let messages = attempts.iter().find(|attempt| {
        attempt.first_divergent_left_message.is_some()
            || attempt.first_divergent_right_message.is_some()
    });
    DivergencePosition {
        scheduler_turn: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_scheduler_turn),
        virtual_nanoseconds: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_virtual_nanoseconds),
        record: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_record),
        syscall: attempts
            .iter()
            .find_map(|attempt| attempt.first_divergent_syscall),
        left_message: messages.and_then(|attempt| attempt.first_divergent_left_message.clone()),
        right_message: messages.and_then(|attempt| attempt.first_divergent_right_message.clone()),
    }
}

pub(crate) fn summarize_sabre_path_evidence(
    attempts: &[AttemptResult],
) -> Result<Option<JsonValue>, String> {
    let is_sabre = attempts.iter().any(|attempt| {
        attempt
            .argv
            .windows(2)
            .any(|window| window[0] == "--backend" && window[1] == "sabre")
    });
    if !is_sabre {
        return Ok(None);
    }
    let mut executions = Vec::<PathEvidence>::new();
    for line in attempts
        .iter()
        .flat_map(|attempt| attempt.sabre_path_evidence.as_deref().unwrap_or("").lines())
    {
        let value = serde_json::from_str::<JsonValue>(line)
            .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?;
        if value.get("schema").and_then(JsonValue::as_u64) != Some(u64::from(PathEvidence::SCHEMA))
        {
            return Err(format!(
                "invalid SaBRe path-evidence record: schema must be {}",
                PathEvidence::SCHEMA
            ));
        }
        executions.push(
            serde_json::from_value::<PathEvidence>(value)
                .map_err(|error| format!("invalid SaBRe path-evidence record: {error}"))?,
        );
    }
    let expected = attempts
        .iter()
        // A parity cell carries a ptrace reference attempt beside the SaBRe
        // candidate. Only executions which actually selected SaBRe can emit
        // SaBRe path evidence; counting the ptrace operand would turn every
        // otherwise-valid SaBRe parity measurement into an incomplete path.
        .filter(|attempt| {
            attempt
                .argv
                .windows(2)
                .any(|window| window[0] == "--backend" && window[1] == "sabre")
        })
        .map(|attempt| {
            if attempt.argv.iter().any(|arg| arg == "--verify") {
                2
            } else {
                1
            }
        })
        .sum::<usize>();
    let complete = executions.len() == expected;
    let mut guest_rpc_observed = complete;
    let mut ptrace_fallback_sites = 0usize;
    let mut trusted_shared_object_sites = 0usize;
    let mut trusted_shared_objects = BTreeSet::new();
    for row in &executions {
        // Keep this pattern exhaustive. Adding a producer field must stop this
        // consumer at compile time until its meaning is handled here.
        let PathEvidence {
            schema,
            guest_rpc_observed: observed,
            ptrace_fallback_sites: fallback_sites,
            trusted_shared_object_sites: shared_object_sites,
            trusted_shared_objects: shared_objects,
        } = row;
        if *schema != PathEvidence::SCHEMA {
            return Err(format!(
                "invalid SaBRe path-evidence record: schema must be {}",
                PathEvidence::SCHEMA
            ));
        }
        guest_rpc_observed &= *observed;
        ptrace_fallback_sites += *fallback_sites;
        trusted_shared_object_sites += *shared_object_sites;
        trusted_shared_objects.extend(shared_objects.iter().cloned());
    }
    let eligible = complete
        && guest_rpc_observed
        && ptrace_fallback_sites == 0
        && trusted_shared_object_sites == 0;
    Ok(Some(serde_json::json!({
        "schema": 1,
        "expected_execution_count": expected,
        "complete": complete,
        "execution_count": executions.len(),
        "guest_rpc_observed": guest_rpc_observed,
        "ptrace_fallback_sites": ptrace_fallback_sites,
        "trusted_shared_object_sites": trusted_shared_object_sites,
        "trusted_shared_objects": trusted_shared_objects,
        "eligible": eligible,
        "executions": executions
    })))
}

fn diversity_evidence(
    hashes: &[String],
    outcome_classes: Option<u64>,
    min_distinct: u64,
    min_normalized_entropy: Option<f64>,
) -> JsonValue {
    let mut counts = BTreeMap::<&str, u64>::new();
    for hash in hashes {
        *counts.entry(hash).or_default() += 1;
    }
    let total = hashes.len() as f64;
    let entropy_bits = counts.values().fold(0.0, |entropy, count| {
        let share = *count as f64 / total.max(1.0);
        if share > 0.0 {
            entropy - share * share.log2()
        } else {
            entropy
        }
    });
    let normalized_entropy = outcome_classes
        .filter(|classes| *classes >= 2)
        .map(|classes| entropy_bits / (classes as f64).log2())
        .unwrap_or(0.0);
    let minority_share = counts
        .values()
        .min()
        .map(|count| *count as f64 / total.max(1.0))
        .unwrap_or(0.0);
    let histogram = counts
        .iter()
        .map(|(hash, count)| format!("{}:{count}", &hash[..hash.len().min(12)]))
        .collect::<Vec<_>>();
    serde_json::json!({
        "distinct": counts.len(), "outcome_classes": outcome_classes, "seeds": hashes.len(),
        "entropy_bits": entropy_bits, "normalized_entropy": normalized_entropy,
        "minority_share": minority_share,
        "oracle_saturated": outcome_classes.is_some_and(|classes| min_distinct >= classes),
        "class_histogram": histogram,
        "min_normalized_entropy": min_normalized_entropy,
    })
}

pub fn prepare_result_path(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn append_result(path: &Path, result: &CellResult) -> Result<(), String> {
    result.require_current_classification()?;
    result.require_current_timeout_policy()?;
    result.require_cpu_observations()?;
    // A missing prerequisite means the cell did not execute. Keep the typed
    // value for the harness summary and JUnit skip, but do not publish a cell
    // row that downstream readers could count as an observation. The validate
    // record names the withheld node and prerequisite separately.
    if result.outcome == "HOST-INAPPLICABLE" {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    // Serialize the row in memory and append it, newline included, in one
    // write. Serializing straight into the unbuffered file made one write(2)
    // per JSON token, about 2,000 for each row.
    let mut row = serde_json::to_vec(result).map_err(|e| e.to_string())?;
    row.push(b'\n');
    file.write_all(&row).map_err(|e| e.to_string())?;
    // The bucket runner publishes each completed cell before printing its
    // PASS/FAIL/ERROR line. Flush the row now rather than waiting for the
    // bucket's JUnit/summary epilogue, which an outer node timeout may kill.
    file.flush().map_err(|e| e.to_string())
}

pub fn write_junit(path: &Path, results: &[CellResult]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let failures = results
        .iter()
        .filter(|result| result.outcome == "FAIL")
        .count();
    let errors = results
        .iter()
        .filter(|result| result.outcome == "ERROR")
        .count();
    let skipped = results
        .iter()
        .filter(|result| result.outcome == "HOST-INAPPLICABLE")
        .count();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"hermit-e2e\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\">\n",
        results.len()
    );
    for result in results {
        let time = result
            .duration_ms
            .map(|duration_ms| format!(" time=\"{:.3}\"", duration_ms as f64 / 1000.0))
            .unwrap_or_default();
        out.push_str(&format!(
            "  <testcase classname=\"{}\" name=\"{}/{}/{}\"{}>",
            xml(&result.category),
            xml(&result.test),
            xml(&result.mode),
            xml(result.backend.as_deref().unwrap_or("none")),
            time,
        ));
        if result.outcome == "FAIL" {
            out.push_str(&format!(
                "<failure>{}</failure>",
                xml(result.reason_for_display())
            ));
        }
        if result.outcome == "ERROR" {
            out.push_str(&format!(
                "<error>{}</error>",
                xml(result.reason_for_display())
            ));
        }
        if result.outcome == "HOST-INAPPLICABLE" {
            out.push_str(&format!(
                "<skipped message=\"{}\"/>",
                xml(result.reason_for_display())
            ));
        }
        out.push_str("</testcase>\n");
    }
    out.push_str("</testsuite>\n");
    fs::write(path, out).map_err(|e| e.to_string())
}

fn prepare_dirs(root: &Path, dir: &Path) -> Result<(), String> {
    for child in [
        "home",
        "xdg-config",
        "tmp",
        "fixtures",
        "recording",
        "captures",
    ] {
        fs::create_dir_all(dir.join(child)).map_err(|e| e.to_string())?;
    }
    let xdg = root.join("tests/e2e/xdg-config");
    if xdg.is_dir() {
        copy_tree(&xdg, &dir.join("xdg-config"))?;
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = destination.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn cell_env(dir: &Path, verified: bool) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        (
            "HOME".into(),
            dir.join("home").to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME".into(),
            dir.join("xdg-config").to_string_lossy().into_owned(),
        ),
        (
            "E2E_TMPDIR".into(),
            if verified {
                "/tmp/hermit-e2e".into()
            } else {
                dir.join("tmp").to_string_lossy().into_owned()
            },
        ),
        (
            "E2E_FIXTURE_DIR".into(),
            dir.join("fixtures").to_string_lossy().into_owned(),
        ),
    ])
}

fn execution_cell_env(
    context: &RunContext,
    dir: &Path,
    verified: bool,
) -> BTreeMap<String, String> {
    let mut env = cell_env(dir, verified);
    env.insert(
        SCHEDULED_JOBS_ENV.into(),
        context.scheduled_worker_capacity.configured().to_string(),
    );
    env
}

fn preparation_env(dir: &Path) -> BTreeMap<String, String> {
    let mut env = cell_env(dir, false);
    let ambient_home = std::env::var_os("HOME");
    let rustup_home = std::env::var_os("RUSTUP_HOME");
    let cargo_home = std::env::var_os("CARGO_HOME");
    add_toolchain_homes(
        &mut env,
        rustup_home.as_deref(),
        cargo_home.as_deref(),
        ambient_home.as_deref(),
    );
    env
}

fn add_toolchain_homes(
    env: &mut BTreeMap<String, String>,
    rustup_home: Option<&OsStr>,
    cargo_home: Option<&OsStr>,
    ambient_home: Option<&OsStr>,
) {
    // Guest preparation is a build-time context, not guest execution. Preserve
    // the invoking toolchain state even though the cell's HOME is isolated;
    // these values are deliberately absent from `cell_env` and never become
    // Hermit `--env` arguments.
    for (name, explicit, fallback) in [
        ("RUSTUP_HOME", rustup_home, ".rustup"),
        ("CARGO_HOME", cargo_home, ".cargo"),
    ] {
        let value = explicit
            .map(PathBuf::from)
            .or_else(|| ambient_home.map(|home| PathBuf::from(home).join(fallback)));
        if let Some(value) = value {
            env.insert(name.into(), value.to_string_lossy().into_owned());
        }
    }
}

/// Render recorded command fields without executing or expanding their contents.
pub fn shell_command(cwd: &str, env: &BTreeMap<String, String>, argv: &[String]) -> String {
    let mut words = vec!["cd".into(), shell_quote(cwd), "&&".into(), "env".into()];
    words.extend(
        env.iter()
            .map(|(name, value)| shell_quote(&format!("{name}={value}"))),
    );
    words.extend(argv.iter().map(|arg| shell_quote(arg)));
    words.join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

/// The guest environment the runner forwards to every Hermit cell. A
/// manifest's `env` may add variables but never set one of these.
const RUNNER_GUEST_ENV: [&str; 7] = [
    "LC_ALL",
    "TZ",
    "HOME",
    "XDG_CONFIG_HOME",
    "E2E_TMPDIR",
    "E2E_FIXTURE_DIR",
    SCHEDULED_JOBS_ENV,
];

/// The Hermit `run` flags a manifest's `hermit_args` may add: exactly those a
/// recorded test policy needs. Each relaxes determinism; an entry ending in `=`
/// takes a value.
const ALLOWED_HERMIT_ARGS: &[&str] = &["--no-virtualize-cpuid", "--max-timeslice="];

/// Validate a mode's `hermit_args`, `env` and `comparator`.
fn validate_mode_extensions(id: &str, mode: &str, recipe: &ModeRecipe) -> Result<(), String> {
    let extends = !recipe.hermit_args.is_empty()
        || recipe.hermit_args_reason.is_some()
        || !recipe.env.is_empty()
        || recipe.comparator.is_some()
        || recipe.comparator_reason.is_some()
        || !recipe.diagnostic.is_empty()
        || recipe.no_retry_reason.is_some();
    if extends && mode != "verify" {
        return Err(format!(
            "{id}: {mode} declares hermit_args, env, comparator, diagnostic or no_retry_reason, which only a verify mode accepts"
        ));
    }
    if recipe
        .no_retry_reason
        .as_deref()
        .is_some_and(|reason| reason.trim().is_empty())
    {
        return Err(format!("{id}: no_retry_reason must be substantive"));
    }
    // Each flag at most once. An exact repeat would repeat a relaxation
    // identity, which the scorecard refuses; two values for one flag would
    // leave the effective configuration to Hermit's argument order.
    let mut seen_flags = BTreeSet::new();
    for arg in &recipe.hermit_args {
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        if !seen_flags.insert(flag) {
            return Err(format!("{id}: hermit_args repeats `{flag}`"));
        }
    }
    for arg in &recipe.hermit_args {
        let allowed = ALLOWED_HERMIT_ARGS
            .iter()
            .any(|flag| match flag.strip_suffix('=') {
                Some(_) => arg
                    .strip_prefix(flag)
                    .is_some_and(|value| !value.is_empty()),
                None => arg == flag,
            });
        if !allowed {
            return Err(format!(
                "{id}: hermit_args entry `{arg}` is not one of {ALLOWED_HERMIT_ARGS:?}"
            ));
        }
    }
    match (
        recipe.hermit_args.is_empty(),
        recipe.hermit_args_reason.as_deref().map(str::trim),
    ) {
        (true, None) => {}
        (false, Some(reason)) if !reason.is_empty() => {}
        (false, _) => {
            return Err(format!(
                "{id}: hermit_args relax determinism and require a substantive hermit_args_reason"
            ));
        }
        (true, Some(_)) => {
            return Err(format!("{id}: hermit_args_reason without hermit_args"));
        }
    }
    for (name, value) in &recipe.env {
        let well_formed = name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_uppercase() || first == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !well_formed || value.contains('\0') {
            return Err(format!(
                "{id}: env entry `{name}` is not a valid NAME=VALUE"
            ));
        }
        if RUNNER_GUEST_ENV.contains(&name.as_str()) {
            return Err(format!(
                "{id}: env entry `{name}` is a variable the runner sets itself"
            ));
        }
    }
    let stripped = recipe.comparator == Some(Comparator::Stripped);
    match (stripped, recipe.comparator_reason.as_deref().map(str::trim)) {
        (false, None) => {}
        (true, Some(reason)) if !reason.is_empty() => {}
        (true, _) => {
            return Err(format!(
                "{id}: comparator stripped is below L2 and requires a substantive comparator_reason"
            ));
        }
        (false, Some(_)) => {
            return Err(format!(
                "{id}: comparator_reason without comparator stripped"
            ));
        }
    }
    if stripped
        && recipe
            .assert
            .as_ref()
            .and_then(|assert| assert.bitwise_parity)
            == Some(true)
    {
        return Err(format!(
            "{id}: comparator stripped cannot establish the bitwise_parity it asserts"
        ));
    }
    for (backend, reason) in &recipe.diagnostic {
        if !stripped {
            return Err(format!(
                "{id}: diagnostic is only for comparator stripped cells; a canonical cell's failure must fail the run"
            ));
        }
        if !recipe.backends_enabled.contains(backend) {
            return Err(format!(
                "{id}: diagnostic names backend `{backend}`, which is not in backends_enabled"
            ));
        }
        if reason.trim().is_empty() || reason.trim() != reason {
            return Err(format!(
                "{id}: diagnostic on `{backend}` requires a substantive, trimmed reason"
            ));
        }
    }
    Ok(())
}

/// Validate a test's run-type labels: lowercase words joined by `-`, unique.
fn validate_labels(id: &str, labels: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for label in labels {
        let well_formed = !label.is_empty()
            && label.split('-').all(|word| {
                !word.is_empty()
                    && word
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            });
        if !well_formed {
            return Err(format!(
                "{id}: label `{label}` is not lowercase-words-with-dashes"
            ));
        }
        if !seen.insert(label.as_str()) {
            return Err(format!("{id}: label `{label}` is repeated"));
        }
    }
    Ok(())
}

fn append_guest_env_args(
    argv: &mut Vec<String>,
    env: &BTreeMap<String, String>,
    uses_test_workdir: bool,
) {
    // Every forwarded value is harness-authored, never inherited ambient state.
    // PWD and OLDPWD remain absent; E2E_TMPDIR joins the fresh /test mount while
    // HOME/XDG retain their unique per-cell directories and seeded config.
    for name in RUNNER_GUEST_ENV {
        let value = if uses_test_workdir && name == "E2E_TMPDIR" {
            HERMETIC_TEST_WORKDIR
        } else {
            env.get(name)
                .expect("cell environment contains every forwarded guest value")
        };
        argv.push("--env".into());
        argv.push(format!("{name}={value}"));
    }
}

fn append_scheduled_jobs_env_arg(argv: &mut Vec<String>, env: &BTreeMap<String, String>) {
    let value = env
        .get(SCHEDULED_JOBS_ENV)
        .expect("cell environment contains scheduled worker capacity");
    argv.push("--env".into());
    argv.push(format!("{SCHEDULED_JOBS_ENV}={value}"));
}

fn require_minimal_base_env(argv: &mut Vec<String>) -> Result<(), String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        if let Some(value) = argv[index].strip_prefix("--base-env=") {
            values.push(value.to_owned());
        } else if argv[index] == "--base-env" {
            index += 1;
            let value = argv.get(index).ok_or_else(|| {
                "hermetic validation received --base-env without a value".to_string()
            })?;
            values.push(value.clone());
        }
        index += 1;
    }
    match values.as_slice() {
        [] => argv.push("--base-env=minimal".into()),
        [value] if value == "minimal" => {}
        [value] => {
            return Err(format!(
                "hermetic validation requires --base-env=minimal, got {value}"
            ));
        }
        _ => return Err("hermetic validation received repeated --base-env flags".into()),
    }
    Ok(())
}

/// Choose the guest's working directory. Three sources, in this order, and the
/// ORDER IS THE POINT.
///
///   1. `isolated_workdir` — the hermetic lane's fresh tmpfs at `/test`, enabled by
///      `HERMIT_E2E_EMPTY_WORKDIR` from `ci/hermetic/run-split-validate.sh`.
///   2. `requested_workdir` — a workdir the manifest names for itself.
///   3. `fixed_workdir_source` — the ordinary-host fallback binds a fresh
///      per-attempt directory at `/tmp/test`.
fn append_execution_root_args(
    argv: &mut Vec<String>,
    backend: &str,
    isolated_workdir: Option<&Path>,
    requested_workdir: Option<&str>,
    fixed_workdir_source: Option<&Path>,
) {
    if let Some(workdir) = isolated_workdir {
        // The marked DBT adapter establishes a fresh namespace and /test tmpfs
        // for each physical execution before constructing its runtime. It still
        // refuses the general --mount CLI surface, so pass only the workdir.
        // Other backends create their per-invocation tmpfs from this argument.
        if backend != "dbt" {
            argv.push(format!("--mount=type=tmpfs,target={}", workdir.display()));
        }
        argv.extend(["--workdir".into(), workdir.to_string_lossy().into_owned()]);
    } else if let Some(workdir) = requested_workdir {
        // Outside the explicit hermetic path, a manifest that names its own
        // workdir keeps it and outranks the ordinary-host fallback.
        argv.extend(["--workdir".into(), workdir.into()]);
    } else if let Some(source) = fixed_workdir_source {
        argv.push(format!(
            "--bind={}:{FIXED_GUEST_WORKDIR}",
            source.to_string_lossy()
        ));
        argv.extend(["--workdir".into(), FIXED_GUEST_WORKDIR.into()]);
    }
}

fn observation_hash(observation: &Observation, attempt: &AttemptResult, dir: &Path) -> String {
    let mut digest = Sha256::new();
    if observation.status {
        digest.update(attempt.status.unwrap_or(-1).to_le_bytes());
        digest.update(attempt.signal.unwrap_or(0).to_le_bytes());
    }
    if observation.stdout {
        digest.update(attempt.stdout.as_bytes());
    }
    if observation.stderr {
        digest.update(attempt.stderr.as_bytes());
    }
    for artifact in &observation.artifacts {
        if let Ok(bytes) = fs::read(dir.join("tmp").join(artifact)) {
            digest.update(bytes);
        }
    }
    format!("{:x}", digest.finalize())
}

fn test_digest(root: &Path, test: &TestRecipe) -> Result<String, String> {
    let bytes = if let Some(program) = &test.program {
        fs::read(root.join(program)).map_err(|e| e.to_string())?
    } else {
        serde_json::to_vec(&test.direct.as_ref().map(|direct| match direct {
            DirectCommand::Shell(s) => vec![s.clone()],
            DirectCommand::Argv(v) => v.clone(),
        }))
        .map_err(|e| e.to_string())?
    };
    Ok(hex_digest(&bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The SHA-256 of the file at `path`, or `None` when it cannot be read.
/// It streams the file rather than reading it whole: every cell hashes the
/// Hermit binary three times, and a debug Hermit is about 450 MB, so
/// `fs::read` held that much per concurrently running cell.
fn file_digest(path: &Path) -> Option<String> {
    let mut digest = Sha256::new();
    std::io::copy(&mut fs::File::open(path).ok()?, &mut digest).ok()?;
    Some(format!("{:x}", digest.finalize()))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Ask the hermit binary which commit it was built from.
///
/// The producer-owned `version --json` record is the authority. Human-readable
/// `--version` output is presentation and must not be parsed into provenance.
/// `None` means the binary could not be run, refused the current schema, or
/// returned an invalid revision; it is deliberately distinct from any value.
fn probe_binary_build_sha(program: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(["version", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_binary_build_info(&output.stdout)
}

fn parse_binary_build_info(bytes: &[u8]) -> Option<String> {
    let report = serde_json::from_slice::<detcore_model::build_info::BuildInfo>(bytes).ok()?;
    if report.schema != detcore_model::build_info::BuildInfo::SCHEMA {
        return None;
    }
    let sha = report.git_sha;
    let hex = sha.strip_suffix("-dirty").unwrap_or(&sha);
    let recognised =
        hex == "unknown" || (hex.len() >= 7 && hex.chars().all(|c| c.is_ascii_hexdigit()));
    recognised.then_some(sha)
}

/// Whether `program args` advertises `needle` in its help output.
///
/// This decides which verification spelling the harness passes. The previous
/// implementation collapsed three distinct outcomes into one `bool`: the
/// needle is absent, the binary could not be launched, and the binary exited
/// non-zero all returned `false`. `false` means `--verify-strict` is omitted,
/// and on a pre-cutover Hermit bare `--verify` is not strict -- so a probe
/// that merely FAILED silently ran the whole suite with weaker verification
/// than intended, and every cell still passed. The failure mode produced the
/// reassuring answer.
///
/// The split is by whether the binary will go on to run cells:
///
/// - The binary does not exist. Nothing will run, and every cell reports
///   `cannot execute <path>` exactly, so reporting absence here cannot weaken
///   a comparison that never happens. Answering `false` keeps that diagnosis
///   with the execution path that states it precisely.
/// - The binary exists but the probe did not answer -- it exited non-zero, or
///   could not be launched for any reason other than absence. Cells WILL run,
///   so guessing the weaker spelling here is exactly the silent downgrade.
///   Refuse instead.
fn command_help_contains(program: &Path, args: &[&str], needle: &str) -> Result<bool, String> {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        // The execution path reports an absent binary precisely; see above.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "cannot launch {} {} to detect {needle} support: {error}; \
                 refusing to guess the verification spelling",
                program.display(),
                args.join(" ")
            ));
        }
    };
    if !output.status.success() {
        return Err(format!(
            "{} {} exited {} while detecting {needle} support; \
             refusing to guess the verification spelling",
            program.display(),
            args.join(" "),
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_string(), |code| code.to_string()),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).contains(needle)
        || String::from_utf8_lossy(&output.stderr).contains(needle))
}

fn execute_observed_until(
    spec: &CellRunSpec,
    observation: &Observation,
    dir: &Path,
    budget: ExecutionBudget,
    observations: &mut Vec<InvocationCpuObservation>,
) -> Result<AttemptResult, String> {
    let mut attempt = execute_spec_until(
        spec,
        budget.deadline,
        budget.cpu_timeout_seconds,
        budget.wall_timeout_seconds,
        budget.remaining_cpu_usec,
        observations,
    )?;
    attempt.observation_sha256 = Some(observation_hash(observation, &attempt, dir));
    Ok(attempt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci_selection::BackendCiDisabledReason;

    #[test]
    fn source_sha_must_be_one_full_lowercase_commit() {
        assert!(validate_source_sha("03bbb83581fad247251df6363f50e61e24c2957e").is_ok());
        for refused in [
            "03bbb835",
            "03BBB83581FAD247251DF6363F50E61E24C2957E",
            "03bbb83581fad247251df6363f50e61e24c2957e\n",
            "g3bbb83581fad247251df6363f50e61e24c2957e",
            "",
        ] {
            let error = validate_source_sha(refused).unwrap_err();
            assert!(error.contains("git rev-parse HEAD"), "{error}");
        }
    }

    /// A source snapshot names its commit; the run must not ask git, which a
    /// `git archive` root cannot answer. Without the name, git stays the
    /// authority and its failure is still an error.
    #[test]
    fn a_source_sha_replaces_git_for_a_snapshot_root() {
        let root = std::env::temp_dir().join(format!("runner-snapshot-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let sha = "03bbb83581fad247251df6363f50e61e24c2957e";
        let named = source_identity(&root, Some(sha));
        let asked = source_identity(&root, None);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(named, Ok((sha.to_string(), false)));
        assert!(asked.is_err(), "{asked:?}");
    }

    /// The four outcomes of the verification-spelling probe.
    ///
    /// The first two are answers and must be reported as answers. The last two
    /// are failures to ask, and the whole point of this test is that they must
    /// NOT be reported as "the flag is absent" -- that reading omits
    /// `--verify-strict`, which on a pre-cutover Hermit silently runs the suite
    /// with weaker verification while every cell still passes.
    ///
    /// Reverting `command_help_contains` to the previous
    /// `.ok().filter(success).is_some_and(..)` form makes the two failure cases
    /// below return `Ok(false)` and fail; the two answer cases keep passing,
    /// because the defect was never in the answering path.
    #[test]
    fn the_capability_probe_separates_an_answer_from_a_failure_to_ask() {
        let needle = "--verify-strict";

        // Ran and the flag is advertised.
        assert_eq!(
            command_help_contains(Path::new("/bin/echo"), &[needle], needle),
            Ok(true),
        );

        // Ran and the flag is genuinely absent. This is the only legitimate
        // route to omitting the flag.
        assert_eq!(
            command_help_contains(Path::new("/bin/echo"), &["--canonical-only"], needle),
            Ok(false),
        );

        // The binary does not exist. No cell will run, and the execution path
        // reports the absent binary exactly, so absence here is safe and is
        // what `production_run_retries_only_product_failures` depends on.
        assert_eq!(
            command_help_contains(
                Path::new("/nonexistent/hermit-probe-target"),
                &["run", "--help"],
                needle,
            ),
            Ok(false),
        );

        // Launched but exited non-zero, so its output is not an answer. This
        // is the dangerous one: such a binary DOES go on to run cells.
        let failed = command_help_contains(Path::new("/bin/false"), &[], needle)
            .expect_err("a probe that exited non-zero must not answer the question");
        assert!(
            failed.contains("exited 1") && failed.contains("refusing to guess"),
            "the refusal must name the exit status, got: {failed}",
        );
    }

    /// The streamed binary digest is the digest of the whole file, across
    /// many read buffers, and an unreadable path has none, as `fs::read`
    /// gave before.
    #[test]
    fn file_digest_matches_a_whole_file_digest() {
        let root = std::env::temp_dir().join(format!("file-digest-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("binary");
        let bytes = (0..300_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        fs::write(&path, &bytes).unwrap();
        assert_eq!(file_digest(&path), Some(hex_digest(&bytes)));
        assert_eq!(file_digest(&root.join("missing")), None);
        assert_eq!(file_digest(&root), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn current_runner_reports_refuse_duplicate_fields() {
        let mut report = VerificationReport::no_result();
        report.no_result_reason = None;
        let raw = serde_json::to_string(&report).unwrap();
        current_verification_report(raw.as_bytes()).unwrap();
        for (needle, replacement) in [
            (r#""verified":false"#, r#""verified":true,"verified":false"#),
            (
                r#""verified":false"#,
                r#""verified":false,"verified":false"#,
            ),
            (
                r#""no_result_reason":null"#,
                r#""no_result_reason":{"kind":"not_run"},"no_result_reason":null"#,
            ),
            (
                r#""no_result_reason":null"#,
                r#""no_result_reason":null,"no_result_reason":null"#,
            ),
        ] {
            assert_eq!(raw.matches(needle).count(), 1);
            let duplicated = raw.replacen(needle, replacement, 1);
            assert!(
                current_verification_report(duplicated.as_bytes())
                    .unwrap_err()
                    .contains("duplicate field")
            );
        }
    }

    #[test]
    fn verification_reports_refuse_duplicate_compared_output_fields() {
        let report = parity_fixture_verification(Verdict::Matched);
        let value = serde_json::to_value(&report).unwrap();
        let raw = serde_json::to_string(&value).unwrap();
        assert_eq!(current_verification_report(raw.as_bytes()).unwrap(), report);
        let outputs = serde_json::to_string(&value["compared_outputs"]).unwrap();
        let mut duplicates = Vec::new();
        for first in ["null", outputs.as_str()] {
            duplicates.push(raw.replacen(
                &format!("\"compared_outputs\":{outputs}"),
                &format!("\"compared_outputs\":{first},\"compared_outputs\":{outputs}"),
                1,
            ));
        }
        for side in ["left", "right"] {
            let output = &value["compared_outputs"][side];
            let original = serde_json::to_string(output).unwrap();
            for field in [
                "exit_code",
                "signal",
                "stdout_sha256",
                "stdout_bytes",
                "stderr_sha256",
                "stderr_bytes",
            ] {
                let field_value = serde_json::to_string(&output[field]).unwrap();
                let conflicting = match field {
                    "exit_code" | "signal" => "7",
                    "stdout_bytes" | "stderr_bytes" => "123",
                    _ => "\"different-digest\"",
                };
                for first in [field_value.as_str(), conflicting] {
                    let changed = original.replacen(
                        &format!("\"{field}\":{field_value}"),
                        &format!("\"{field}\":{first},\"{field}\":{field_value}"),
                        1,
                    );
                    let changed_outputs = outputs.replacen(
                        &format!("\"{side}\":{original}"),
                        &format!("\"{side}\":{changed}"),
                        1,
                    );
                    duplicates.push(raw.replacen(&outputs, &changed_outputs, 1));
                }
            }
        }
        for duplicated in duplicates {
            assert_ne!(duplicated, raw);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&duplicated).unwrap(),
                value
            );
            assert!(
                current_verification_report(duplicated.as_bytes())
                    .unwrap_err()
                    .contains("duplicate field")
            );
        }
    }

    #[test]
    fn failure_class_schema_matches_serialized_enum() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../failure-class-schema.json")).unwrap();
        assert_eq!(schema["schema"], 1);
        let classes = [
            FailureClass::ProductFailure,
            FailureClass::UnderstoodInfrastructureFailure,
            FailureClass::UnderstoodPrerequisiteFailure,
            FailureClass::NoResult,
        ];
        let serialized = classes
            .iter()
            .map(|class| serde_json::to_value(class).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(schema["values"], serde_json::Value::Array(serialized));
        for class in classes {
            assert_eq!(FailureClass::parse(class.as_str()), Ok(class));
        }
        assert_eq!(
            FailureClass::parse("future_failure"),
            Err("unknown failure_class `future_failure`".into())
        );
    }
    use crate::ci_selection::CiDisabledResult;

    #[test]
    fn retry_outcome_preserves_recovery_and_product_failure() {
        assert_eq!(
            outcome_after_retries([(1, "FAIL"), (2, "PASS")]).unwrap(),
            "PASS"
        );
        assert_eq!(
            outcome_after_retries([(1, "ERROR"), (2, "PASS")]).unwrap(),
            "PASS"
        );
        assert_eq!(
            outcome_after_retries([(1, "FAIL"), (2, "ERROR")]).unwrap(),
            "FAIL"
        );
        assert_eq!(
            outcome_after_retries([(1, "ERROR"), (2, "ERROR")]).unwrap(),
            "ERROR"
        );
        assert_eq!(
            outcome_after_retries([(1, "HOST-INAPPLICABLE")]).unwrap(),
            "HOST-INAPPLICABLE"
        );
    }

    #[test]
    fn retry_outcome_refuses_malformed_history_by_name() {
        for (attempts, named) in [
            (vec![], "no attempts"),
            (vec![(2, "FAIL")], "expected 1"),
            (
                vec![(1, "PASS"), (2, "FAIL")],
                "follows terminal outcome PASS",
            ),
            (
                vec![(1, "HOST-INAPPLICABLE"), (2, "ERROR")],
                "follows terminal outcome HOST-INAPPLICABLE",
            ),
            (vec![(1, "UNKNOWN")], "unknown outcome \"UNKNOWN\""),
            (
                vec![(1, "FAIL"), (2, "ERROR"), (3, "PASS")],
                "exceeds the shared maximum of 2",
            ),
        ] {
            let error = outcome_after_retries(attempts).unwrap_err();
            assert!(error.contains(named), "{error:?} did not name {named:?}");
        }
    }

    #[test]
    fn selected_retry_row_keeps_its_ordinal_while_summary_keeps_history_count() {
        let mut first = cell_result_that_located_nothing();
        first.outcome = "FAIL".into();
        first.attempt = 1;

        let mut infrastructure = first.clone();
        infrastructure.outcome = "ERROR".into();
        infrastructure.attempt = 2;
        let product_then_infrastructure = [first.clone(), infrastructure];
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(&product_then_infrastructure).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("FAIL", 1, 2)
        );

        let mut recovered = first.clone();
        recovered.outcome = "PASS".into();
        recovered.attempt = 2;
        let recovered_history = [first, recovered];
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(&recovered_history).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("PASS", 2, 2)
        );

        let peer = cell_result_that_located_nothing();
        let (selected, attempts) =
            cell_result_and_attempts_after_retries(std::slice::from_ref(&peer)).unwrap();
        assert_eq!(
            (selected.outcome.as_str(), selected.attempt, attempts),
            ("PASS", 1, 1)
        );
    }

    fn recipe(ci: bool) -> TestRecipe {
        let disabled = BTreeMap::from([
            ("dbt".into(), "not qualified".into()),
            ("kvm".into(), "not qualified".into()),
            ("sabre".into(), "not qualified".into()),
            ("liteinst".into(), "not qualified".into()),
        ]);
        let mode = ModeRecipe {
            ci: CiSelectionSpec::Uniform(ci),
            ci_disabled_reason: (!ci)
                // A real sentence, because the shared-string form now carries the
                // same substance rule as a per-backend entry. "not selected yet"
                // is one of the placeholder phrases that rule bans outright.
                .then(|| {
                    CiDisabledReasonSpec::Uniform(
                        "fixture cell: ptrace only, other backends unmeasured here".into(),
                    )
                }),
            backends_enabled: vec!["ptrace".into()],
            backends_disabled: disabled,
            ..ModeRecipe::default()
        };
        TestRecipe {
            id: "fixture/test".into(),
            description: "fixture".into(),
            lane: "portable".into(),
            requires: Vec::new(),
            occasional: false,
            program: None,
            direct: Some(DirectCommand::Argv(vec!["/bin/true".into()])),
            observation: Observation {
                status: true,
                stdout: true,
                stderr: true,
                artifacts: Vec::new(),
            },
            build: None,
            modes: BTreeMap::from([("verify".into(), mode)]),
            preprocessors: Vec::new(),
            labels: Vec::new(),
        }
    }

    #[test]
    fn verify_extensions_are_verify_only_reasoned_and_allowlisted() {
        let accepted = ModeRecipe {
            backends_enabled: vec!["ptrace".into()],
            hermit_args: vec![
                "--no-virtualize-cpuid".into(),
                "--max-timeslice=disabled".into(),
            ],
            hermit_args_reason: Some("the corpus records this configuration".into()),
            env: BTreeMap::from([("TMPDIR".into(), "/tmp".into())]),
            comparator: Some(Comparator::Stripped),
            comparator_reason: Some("the corpus verdict policy is the stripped comparison".into()),
            ..ModeRecipe::default()
        };
        validate_mode_extensions("fixture/test", "verify", &accepted).unwrap();
        let refused = |change: &dyn Fn(&mut ModeRecipe), mode: &str, needle: &str| {
            let mut recipe = accepted.clone();
            change(&mut recipe);
            let error = validate_mode_extensions("fixture/test", mode, &recipe).unwrap_err();
            assert!(error.contains(needle), "{error}");
        };
        refused(&|_| {}, "replay", "only a verify mode accepts");
        // Anything outside the allowlist, including every flag that would
        // weaken the comparison or determinism by another route.
        for flag in [
            "--no-detlog-io-buffers",
            "--no-rcb-time",
            "--no-strict",
            "--verify-allow=failure",
            "--verify-verbose",
            "--no-virtualize-time",
            "--chaos",
            "--epoch=1",
            "--max-timeslice=",
            "--no-virtualize-cpuid=1",
            "true",
        ] {
            refused(
                &|r| r.hermit_args = vec![flag.into()],
                "verify",
                "is not one of",
            );
        }
        refused(
            &|r| {
                r.hermit_args = vec![
                    "--no-virtualize-cpuid".into(),
                    "--no-virtualize-cpuid".into(),
                ]
            },
            "verify",
            "repeats `--no-virtualize-cpuid`",
        );
        refused(
            &|r| {
                r.hermit_args = vec![
                    "--max-timeslice=1".into(),
                    "--max-timeslice=disabled".into(),
                ]
            },
            "verify",
            "repeats `--max-timeslice`",
        );
        refused(
            &|r| r.hermit_args_reason = None,
            "verify",
            "require a substantive hermit_args_reason",
        );
        refused(
            &|r| r.hermit_args_reason = Some(" ".into()),
            "verify",
            "require a substantive hermit_args_reason",
        );
        refused(
            &|r| r.hermit_args.clear(),
            "verify",
            "hermit_args_reason without hermit_args",
        );
        refused(
            &|r| r.comparator_reason = None,
            "verify",
            "requires a substantive comparator_reason",
        );
        refused(
            &|r| r.no_retry_reason = Some(" ".into()),
            "verify",
            "no_retry_reason must be substantive",
        );
        refused(
            &|r| r.no_retry_reason = Some("single recorded run".into()),
            "replay",
            "only a verify mode accepts",
        );
        refused(
            &|r| r.comparator = None,
            "verify",
            "comparator_reason without comparator stripped",
        );
        refused(
            &|r| {
                r.assert = Some(Assertions {
                    bitwise_parity: Some(true),
                    ..Assertions::default()
                })
            },
            "verify",
            "cannot establish the bitwise_parity",
        );
        for owned in RUNNER_GUEST_ENV {
            refused(
                &|r| r.env = BTreeMap::from([(owned.into(), "x".into())]),
                "verify",
                "a variable the runner sets itself",
            );
        }
        refused(
            &|r| r.env = BTreeMap::from([("lower".into(), "x".into())]),
            "verify",
            "not a valid NAME=VALUE",
        );
        // A diagnostic is accepted only on an enabled backend of a stripped
        // cell, with a substantive reason, and only in verify.
        let diagnostic = || BTreeMap::from([("ptrace".into(), "bounded probe".into())]);
        let mut with_diagnostic = accepted.clone();
        with_diagnostic.diagnostic = diagnostic();
        validate_mode_extensions("fixture/test", "verify", &with_diagnostic).unwrap();
        refused(
            &|r| r.diagnostic = diagnostic(),
            "replay",
            "only a verify mode accepts",
        );
        refused(
            &|r| {
                r.diagnostic = diagnostic();
                r.comparator = None;
                r.comparator_reason = None;
            },
            "verify",
            "a canonical cell's failure must fail the run",
        );
        refused(
            &|r| r.diagnostic = BTreeMap::from([("kvm".into(), "bounded probe".into())]),
            "verify",
            "not in backends_enabled",
        );
        for reason in ["", " ", " padded"] {
            refused(
                &|r| r.diagnostic = BTreeMap::from([("ptrace".into(), reason.into())]),
                "verify",
                "requires a substantive, trimmed reason",
            );
        }
        // The validation is wired into document loading, not only callable.
        let five_modes = || {
            let mut test = recipe(true);
            for mode in ["naked", "replay", "chaos", "custom"] {
                let backends: &[&str] = if mode == "naked" {
                    &["native"]
                } else {
                    &["ptrace", "dbt", "kvm", "sabre", "liteinst"]
                };
                test.modes.insert(
                    mode.into(),
                    ModeRecipe {
                        ci: CiSelectionSpec::Uniform(false),
                        ci_disabled_reason: Some(CiDisabledReasonSpec::Uniform(
                            "fixture cell: only the verify mode is measured here".into(),
                        )),
                        backends_disabled: backends
                            .iter()
                            .map(|backend| ((*backend).into(), "fixture mode is not run".into()))
                            .collect(),
                        ..ModeRecipe::default()
                    },
                );
            }
            test
        };
        let document = ManifestDocument {
            schema: MANIFEST_SCHEMA,
            bucket: "fixture".into(),
            timeout_seconds: None,
            slow_reason: None,
            test: vec![five_modes()],
        };
        validate_document_with_cpu(
            &document,
            "fixture",
            Path::new("/"),
            15,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
        )
        .unwrap();
        let mut test = five_modes();
        test.modes.get_mut("verify").unwrap().hermit_args = vec!["--chaos".into()];
        let document = ManifestDocument {
            test: vec![test],
            ..document
        };
        let error = validate_document_with_cpu(
            &document,
            "fixture",
            Path::new("/"),
            15,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
        )
        .unwrap_err();
        assert!(error.contains("is not one of"), "{error}");
        let mut labelled = five_modes();
        labelled.labels = vec!["Bad".into()];
        let error = validate_document_with_cpu(
            &ManifestDocument {
                test: vec![labelled],
                ..document
            },
            "fixture",
            Path::new("/"),
            15,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS,
        )
        .unwrap_err();
        assert!(
            error.contains("is not lowercase-words-with-dashes"),
            "{error}"
        );
    }

    #[test]
    fn declared_relaxations_are_recorded_on_the_cell() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.hermit_args = vec!["--no-virtualize-cpuid".into()];
        mode.hermit_args_reason = Some("corpus configuration".into());
        mode.comparator = Some(Comparator::Stripped);
        mode.comparator_reason = Some("corpus verdict policy".into());
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        assert_eq!(
            cell_relaxations(&cell),
            [
                "--no-virtualize-cpuid: corpus configuration",
                "comparator=stripped: corpus verdict policy"
            ]
        );
    }

    #[test]
    fn labels_are_lowercase_dashed_words_and_unique() {
        validate_labels("t", &["strict-compat".into(), "full".into(), "rr2".into()]).unwrap();
        for bad in [
            vec!["Full"],
            vec!["a--b"],
            vec!["-a"],
            vec![""],
            vec!["a b"],
            vec!["x", "x"],
        ] {
            let labels = bad.iter().map(|l| l.to_string()).collect::<Vec<_>>();
            assert!(validate_labels("t", &labels).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn labels_select_only_the_tests_that_carry_them() {
        let mut strict = recipe(true);
        strict.id = "fixture/strict".into();
        strict.labels = vec!["strict-compat".into(), "full".into()];
        let mut plain = recipe(true);
        plain.id = "fixture/plain".into();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: [strict, plain]
                .into_iter()
                .map(|test| {
                    (
                        test.id.clone(),
                        ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
                    )
                })
                .collect(),
        };
        let selected = |labels: &[&str]| {
            set.select(&Selection {
                labels: labels.iter().map(|l| l.to_string()).collect(),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .map(|cell| cell.id.test)
            .collect::<Vec<_>>()
        };
        assert_eq!(selected(&[]), ["fixture/plain", "fixture/strict"]);
        assert_eq!(selected(&["full"]), ["fixture/strict"]);
        assert_eq!(selected(&["strict-compat", "full"]), ["fixture/strict"]);
        let error = set
            .select(&Selection {
                labels: vec!["full".into(), "super".into()],
                ..Selection::default()
            })
            .unwrap_err();
        assert_eq!(error, "--label super names no test in any manifest");
    }

    fn extended_verify_spec(comparator: Option<Comparator>) -> CellRunSpec {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.hermit_args = vec!["--no-virtualize-cpuid".into()];
        mode.hermit_args_reason = Some("fixture configuration".into());
        mode.env = BTreeMap::from([("TMPDIR".into(), "/tmp".into())]);
        mode.comparator = comparator;
        mode.comparator_reason = comparator.map(|_| "fixture policy".into());
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap()
    }

    #[test]
    fn a_verify_cell_carries_its_hermit_args_env_and_comparator() {
        let strict = extended_verify_spec(None);
        let at = |argv: &[String], arg: &str| argv.iter().position(|a| a == arg);
        let separator = at(&strict.argv, "--").unwrap();
        let flag = at(&strict.argv, "--no-virtualize-cpuid").unwrap();
        assert!(
            at(&strict.argv, "--strict").unwrap() < flag
                && flag < at(&strict.argv, "--verify").unwrap()
        );
        let env = strict
            .argv
            .windows(2)
            .position(|w| w == ["--env", "TMPDIR=/tmp"])
            .unwrap();
        let home = strict
            .argv
            .iter()
            .position(|a| a.starts_with("HOME="))
            .unwrap();
        assert!(home < env && env < separator, "{:?}", strict.argv);
        assert!(at(&strict.argv, "--verify-strict").is_some());
        assert_eq!(strict.comparator, Comparator::Strict);

        let stripped = extended_verify_spec(Some(Comparator::Stripped));
        assert!(
            at(&stripped.argv, "--verify-strict").is_none(),
            "{:?}",
            stripped.argv
        );
        assert!(at(&stripped.argv, "--verify").is_some());
        assert_eq!(stripped.comparator, Comparator::Stripped);
    }

    #[test]
    fn a_relative_hermit_bin_names_a_checkout_path() {
        let root = Path::new("/repo");
        assert_eq!(
            resolve_hermit_bin(root, None),
            Path::new("/repo/target/debug/hermit")
        );
        assert_eq!(
            resolve_hermit_bin(
                root,
                Some("ignored/hermetic/split/target/release/hermit".into())
            ),
            Path::new("/repo/ignored/hermetic/split/target/release/hermit")
        );
        assert_eq!(
            resolve_hermit_bin(root, Some("/opt/hermit".into())),
            Path::new("/opt/hermit")
        );
    }

    /// The strict compatibility corpus ran each program as its own validation
    /// node with `hermit run --strict --verify --base-env=minimal
    /// --no-virtualize-cpuid --max-timeslice=disabled
    /// --mount=type=tmpfs,target=/test --workdir=/test --env TMPDIR=/tmp --
    /// <argv> </dev/null`. Fold 1 of
    /// https://github.com/rrnewton/hermit/issues/3448 moved it into
    /// compat.yaml; the shipped row, built the way the runner builds it under
    /// e2e.manifest_compat's HERMIT_E2E_EMPTY_WORKDIR=/test, still carries
    /// every one of those flags, the stripped comparator, and the guest argv
    /// with its run-owned fixture path.
    #[test]
    fn a_shipped_compat_row_keeps_the_strict_compatibility_probe_flags() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                test: Some("compat/cat".into()),
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        let [cell] = cells.as_slice() else {
            panic!("compat/cat must have exactly one CI cell: {cells:?}");
        };
        assert_eq!(
            (cell.id.mode.as_str(), cell.id.backend.as_deref()),
            ("verify", Some("ptrace"))
        );
        let DirectCommand::Argv(argv) = cell.test.direct.as_ref().unwrap() else {
            panic!("compat/cat must be a direct argv");
        };
        let guest = crate::manifest_corpus::resolve_direct_placeholders(
            &cell.test.id,
            argv,
            &root,
            Some(std::ffi::OsStr::new("/run-state")),
        )
        .unwrap();
        assert_eq!(
            guest,
            [
                "/bin/cat",
                "/run-state/strict-compat/real-compat-fixtures/README.md"
            ]
        );
        let context = RunContext {
            root: root.clone(),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: true,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: Some(PathBuf::from("/test")),
        };
        let spec = build_spec(
            &context,
            cell,
            PathBuf::from("/repo/results/cell"),
            guest.clone(),
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let argv = &spec.argv;
        let at = |want: &[&str]| {
            argv.windows(want.len())
                .position(|window| window == want)
                .unwrap_or_else(|| panic!("{want:?} missing from {argv:?}"))
        };
        let separator = at(&["--"]);
        let ordered = [
            at(&["run"]),
            at(&["--base-env=minimal"]),
            at(&["--strict"]),
            at(&["--no-virtualize-cpuid"]),
            at(&["--max-timeslice=disabled"]),
            at(&["--verify"]),
            at(&["--mount=type=tmpfs,target=/test"]),
            at(&["--workdir", "/test"]),
            at(&["--env", "TMPDIR=/tmp"]),
            separator,
        ];
        assert!(ordered.is_sorted(), "{ordered:?} in {argv:?}");
        assert!(!argv.iter().any(|arg| arg == "--verify-strict"), "{argv:?}");
        assert_eq!(argv[separator + 1..], guest[..]);
        assert_eq!(spec.comparator, Comparator::Stripped);
        assert_eq!((cell.timeout_seconds, cell.cpu_timeout_seconds), (60, 59));
        // One attempt, as the generated node had; an ordinary cell retries.
        assert!(!retries_product_failures(cell));
        let ordinary = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                test: Some("c-programs/random-readv-stream".into()),
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        assert!(retries_product_failures(&ordinary[0]));
    }

    use super::PRODUCER_STRIPPED_REPORT;

    #[test]
    fn a_stripped_cell_passes_only_a_matched_report_and_a_strict_one_still_needs_canonical() {
        let matched: VerificationReport = serde_json::from_str(PRODUCER_STRIPPED_REPORT).unwrap();
        assert_eq!(
            matched.comparison.as_ref().unwrap().strictness,
            crate::canonical_verdict::LogCompareStrictness::Stripped
        );
        let pass = attempt_with_report(None, Comparator::Stripped, matched.clone(), "exit 0", 5);
        assert_eq!(pass.outcome, "PASS", "{:?}", pass.reason);
        // The same below-L2 report never passes a strict cell.
        let strict = attempt_with_report(None, Comparator::Strict, matched.clone(), "exit 0", 5);
        assert_ne!(strict.outcome, "PASS", "{:?}", strict.reason);

        // A report without the declared, non-empty stripped comparison is
        // incomplete evidence, not a pass.
        for vacuous in [
            {
                let mut report = matched.clone();
                report.compared_log_messages.as_mut().unwrap().left = 0;
                report
            },
            {
                let mut report = matched.clone();
                report.comparison.as_mut().unwrap().compare_logs = false;
                report
            },
            {
                let mut report = matched.clone();
                report.comparison.as_mut().unwrap().strictness =
                    crate::canonical_verdict::LogCompareStrictness::Canonical;
                report
            },
            {
                let mut report = matched.clone();
                report.compared_log_messages.as_mut().unwrap().right = 0;
                report
            },
            // Without virtual time it is not the comparison a verify cell runs.
            {
                let mut report = matched.clone();
                report.comparison.as_mut().unwrap().virtualize_time = Some(false);
                report
            },
        ] {
            let result = attempt_with_report(None, Comparator::Stripped, vacuous, "exit 0", 5);
            assert_eq!(result.outcome, "ERROR", "{:?}", result.reason);
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("non-vacuous stripped evidence")),
                "{:?}",
                result.reason
            );
        }

        let mut uncompared = matched.clone();
        uncompared.comparison = None;
        let result = attempt_with_report(None, Comparator::Stripped, uncompared, "exit 0", 5);
        assert_eq!(result.outcome, "ERROR", "{:?}", result.reason);

        // Matched logs with different outputs is not a pass either: the
        // report reader already refuses such a report as unreadable evidence.
        let mut outputs_differ = matched.clone();
        outputs_differ
            .compared_outputs
            .as_mut()
            .unwrap()
            .right
            .stdout_bytes += 1;
        let result = attempt_with_report(None, Comparator::Stripped, outputs_differ, "exit 0", 5);
        assert_eq!(result.outcome, "ERROR", "{:?}", result.reason);
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("operands differ")),
            "{:?}",
            result.reason
        );

        let mut diverged = matched;
        diverged.verified = false;
        diverged.verdict = Verdict::Diverged;
        let fail = attempt_with_report(None, Comparator::Stripped, diverged, "exit 1", 5);
        assert_eq!(fail.outcome, "FAIL");
        assert_eq!(
            fail.reason.as_deref(),
            Some("stripped verification did not match: verified=false verdict=diverged")
        );
    }

    /// ⚠️ "NO CELLS" AND "NO SUCH TEST" ARE DIFFERENT ANSWERS, AND `select` GIVES
    /// THE SAME EMPTY VECTOR FOR BOTH. Measured 2026-08-26 before this existed:
    /// `test-harness plan --lane portable --test no-such-test-xyz` printed nothing and
    /// exited 0 -- and so did the same command with a REAL id, so the exit code
    /// carried no information either way and anything bisecting off `plan` read a typo
    /// as "nothing failed here".
    ///
    /// The fix could NOT be `cells.is_empty()`, which is why this predicate exists:
    /// `print_plan` also serves `audit-gaps`, where empty legitimately means NO GAPS.
    /// Measured on the same head, a real id filtered to a lane it has no cells in
    /// (`--lane privileged --test applications/c-toolchain-workflow`) prints `[]` and
    /// exits 0 -- a correct, well-formed query that an emptiness guard would refuse.
    #[test]
    fn knows_test_separates_an_absent_id_from_an_empty_population() {
        let test = recipe(false);
        let id = test.id.clone();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        assert!(set.knows_test(&id), "a declared test must be known");
        assert!(
            !set.knows_test("no-such-test-xyz"),
            "an id in no manifest must NOT be known -- this is the typo a bisect would \
             otherwise read as a pass"
        );
        // The control that makes the pair meaningful: the KNOWN id still selects zero
        // cells in the Required population, so emptiness and unknown-ness genuinely
        // come apart here rather than only in principle.
        assert!(
            set.select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .is_empty(),
            "fixture must be empty in Required, or this test proves nothing"
        );
    }

    fn guest_env_args(argv: &[String]) -> Vec<String> {
        argv.windows(2)
            .filter(|window| window[0] == "--env")
            .map(|window| window[1].clone())
            .collect()
    }

    fn assert_minimal_guest_env(argv: &[String], dir: &str, tmp: &str, jobs: &str) {
        assert_minimal_base_env(argv);
        assert_eq!(
            guest_env_args(argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                format!("HOME={dir}/home"),
                format!("XDG_CONFIG_HOME={dir}/xdg-config"),
                format!("E2E_TMPDIR={tmp}"),
                format!("E2E_FIXTURE_DIR={dir}/fixtures"),
                format!("HERMIT_E2E_SCHEDULED_JOBS={jobs}"),
            ],
            "the guest environment is an exact allowlist; an added, removed, or inherited name is a regression"
        );
        assert!(
            !argv.iter().any(|arg| arg.contains(EQUALIZED_INPUT_ROOT)),
            "a cell that is not equalized binds nothing below {EQUALIZED_INPUT_ROOT}: {argv:?}"
        );
    }

    /// The guest of an equalized verify cell names only guest paths, and each
    /// of the cell's input directories is bound at the path it names.
    fn assert_equalized_guest_env(argv: &[String], dir: &str, tmp: &str, jobs: &str) {
        assert_minimal_base_env(argv);
        assert_eq!(
            guest_env_args(argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                "HOME=/tmp/e2e/home".to_string(),
                "XDG_CONFIG_HOME=/tmp/e2e/xdg-config".to_string(),
                format!("E2E_TMPDIR={tmp}"),
                "E2E_FIXTURE_DIR=/tmp/e2e/fixtures".to_string(),
                format!("HERMIT_E2E_SCHEDULED_JOBS={jobs}"),
            ],
            "the guest environment is an exact allowlist; an added, removed, or inherited name is a regression"
        );
        let separator = argv.iter().position(|arg| arg == "--").unwrap();
        let binds = argv[..separator]
            .iter()
            .filter(|arg| arg.starts_with("--bind=") && arg.contains(EQUALIZED_INPUT_ROOT))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            binds,
            [
                format!("--bind={dir}/home:/tmp/e2e/home"),
                format!("--bind={dir}/xdg-config:/tmp/e2e/xdg-config"),
                format!("--bind={dir}/fixtures:/tmp/e2e/fixtures"),
            ],
            "{argv:?}"
        );
        assert!(
            !argv[..separator]
                .iter()
                .any(|arg| arg.contains(&format!("={dir}/")) && !arg.starts_with("--bind=")),
            "no guest environment value names a host cell path: {argv:?}"
        );
    }

    fn assert_minimal_base_env(argv: &[String]) {
        let mut base_env = Vec::new();
        let mut index = 0;
        while index < argv.len() {
            if let Some(value) = argv[index].strip_prefix("--base-env=") {
                base_env.push(value.to_string());
            } else if argv[index] == "--base-env" {
                index += 1;
                base_env.push(argv[index].clone());
            }
            index += 1;
        }
        assert_eq!(
            base_env,
            ["minimal"],
            "every hermetic Hermit cell must select the minimal base environment exactly once: {argv:?}"
        );
    }

    fn ptrace_cell(mode: &str) -> SelectedCell {
        let mut test = recipe(true);
        if mode != "verify" {
            let mode_recipe = test.modes.remove("verify").unwrap();
            test.modes.insert(mode.into(), mode_recipe);
        }
        SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: mode.into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 600,
            cpu_timeout_seconds: 22,
        }
    }

    fn fixture_host_capabilities() -> HostCapabilities {
        BTreeMap::from([
            (
                HostCapability::CpuidFaulting,
                HostCapabilityVerdict {
                    present: true,
                    evidence: "fixture cpuid probe".into(),
                },
            ),
            (
                HostCapability::Kvm,
                HostCapabilityVerdict {
                    present: false,
                    evidence: "fixture kvm probe".into(),
                },
            ),
        ])
    }

    fn run_context(root: &Path) -> RunContext {
        RunContext {
            root: root.into(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        }
    }

    #[test]
    fn comparison_epoch_is_captured_once_and_explicit_input_never_reads_clock() {
        let calls = std::cell::Cell::new(0);
        let epoch = resolve_run_epoch(None, || {
            calls.set(calls.get() + 1);
            UNIX_EPOCH + Duration::new(978_307_199, 123_456_789)
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(epoch, "2000-12-31T23:59:59.123456789+00:00");
        let explicit = "2026-01-01T00:00:00.987654321Z";
        assert_eq!(
            resolve_run_epoch(Some(explicit.into()), || {
                panic!("an explicit environment epoch must not sample the clock")
            })
            .unwrap(),
            explicit
        );
    }

    #[test]
    fn comparison_epoch_is_retained_across_operands_repeats_and_outer_retries() {
        let root = std::env::temp_dir().join(format!(
            "hermit-comparison-epoch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let mut context = run_context(&root);
        context.epoch = resolve_run_epoch(None, || {
            UNIX_EPOCH + Duration::new(978_307_199, 123_456_789)
        })
        .unwrap();
        for outer_attempt in [1, 2] {
            let context = context.with_attempt(outer_attempt);
            for backend in ["sabre", "ptrace"] {
                let mut cell = ptrace_cell("verify");
                cell.id.backend = Some(backend.into());
                for attempt in ["1", "parity-reference"] {
                    let spec = build_spec(
                        &context,
                        &cell,
                        root.join(backend),
                        vec!["/bin/date".into(), "+%s%N".into()],
                        attempt,
                        None,
                        15,
                    )
                    .unwrap();
                    assert_eq!(spec.env.get("HERMIT_EPOCH"), Some(&context.epoch));
                    assert!(!spec.argv.iter().any(|arg| arg.contains("HERMIT_EPOCH")));
                    assert!(!spec.argv.iter().any(|arg| arg.starts_with("--epoch")));
                }
            }
        }
        let mut custom = ptrace_cell("custom");
        custom.test.modes.get_mut("custom").unwrap().args =
            vec!["--epoch=2026-01-01T00:00:00.987654321Z".into()];
        let spec = build_spec(
            &context,
            &custom,
            root.join("custom"),
            vec!["/bin/true".into()],
            "1",
            None,
            15,
        )
        .unwrap();
        assert_eq!(spec.env.get("HERMIT_EPOCH"), Some(&context.epoch));
        assert_eq!(
            spec.argv
                .iter()
                .filter(|arg| arg.starts_with("--epoch"))
                .collect::<Vec<_>>(),
            [&"--epoch=2026-01-01T00:00:00.987654321Z".to_owned()]
        );
        let naked = build_spec(
            &context,
            &ptrace_cell("naked"),
            root.join("naked"),
            vec!["/bin/true".into()],
            "1",
            None,
            15,
        )
        .unwrap();
        assert!(!naked.env.contains_key("HERMIT_EPOCH"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cell_retry_attempts_use_distinct_artifact_directories() {
        let root = PathBuf::from("/tmp/hermit-cell-retry-artifact-fixture");
        let cell = ptrace_cell("verify");
        let first = run_context(&root);
        let second = first.with_attempt(2);

        let first_dir = cell_artifact_dir(&first, &cell);
        let second_dir = cell_artifact_dir(&second, &cell);
        assert_ne!(first_dir, second_dir);
        assert_eq!(
            first_dir,
            cell_artifact_path(&first.result_root, &first.run_id, &cell.id, 1)
        );
        assert_eq!(first_dir.file_name().unwrap(), "fixture-test-verify-ptrace");
        assert_eq!(
            second_dir,
            cell_artifact_path(&second.result_root, &second.run_id, &cell.id, 2)
        );
        assert!(
            second_dir
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-attempt-2"))
        );
    }

    fn bound_workdir_source(spec: &CellRunSpec) -> PathBuf {
        let bind = spec
            .argv
            .iter()
            .find_map(|arg| {
                arg.strip_prefix("--bind=")
                    .and_then(|arg| arg.strip_suffix(":/tmp/test"))
            })
            .expect("default workdir bind is present");
        PathBuf::from(bind)
    }

    #[test]
    fn required_and_enabled_are_distinct_populations() {
        let test = recipe(false);
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        assert!(
            set.select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            set.select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap()
            .len(),
            1
        );
        let cell = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-classification-bracket-{}",
            std::process::id()
        ));
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        assert_eq!(
            infrastructure_error_result(&context, &cell, "fixture".into()).classification,
            "required"
        );
    }

    #[test]
    fn disabled_population_uses_only_its_explicit_guest_arguments() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.guest_args.insert(
            "ptrace".into(),
            vec!["ptrace-scenario".into(), "ptrace-detail".into()],
        );
        mode.guest_args
            .insert("kvm".into(), vec!["kvm-scenario".into()]);
        validate_mode("fixture/test", "verify", mode, 60).unwrap();

        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 60, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let cells = set
            .select(&Selection {
                population: Some(Population::Disabled),
                test: Some("fixture/test".into()),
                mode: Some("verify".into()),
                backend: Some("kvm".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        assert!(!cells[0].enabled);

        let root = std::env::temp_dir().join(format!(
            "hermit-runner-disabled-guest-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let context = run_context(&root);
        let guest = prepare_test(&context, &cells[0], &root.join("results/cell")).unwrap();
        assert_eq!(guest, ["/bin/true", "kvm-scenario"]);
        assert!(
            !guest.iter().any(|arg| arg == "ptrace-scenario"),
            "an unselected backend must not inherit a selected sibling's arguments"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_explicit_empty_guest_arguments() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.guest_args.insert("kvm".into(), Vec::new());
        validate_mode("fixture/test", "verify", &mode, 60).unwrap();
    }

    #[test]
    fn rejects_nul_in_guest_arguments() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.guest_args
            .insert("kvm".into(), vec!["contains\0nul".into()]);
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 60).unwrap_err(),
            "fixture/test: verify guest_args.kvm contains a NUL byte, which Linux argv cannot represent"
        );
    }

    #[test]
    fn exact_cell_timeout_overrides_the_bucket_and_global_defaults() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.backends_enabled = vec!["ptrace".into(), "liteinst".into()];
        mode.backends_disabled.remove("liteinst");
        mode.timeout_seconds.insert("ptrace".into(), 30);
        mode.cpu_timeout_seconds.insert("ptrace".into(), 25);
        mode.slow_reason.insert(
            "ptrace".into(),
            "three complete validation runs measured this cell above the inherited limit".into(),
        );
        validate_mode("fixture/test", "verify", mode, 20).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 20, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let cells = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.id.backend.as_deref() == Some("ptrace"))
                .unwrap()
                .timeout_seconds,
            30
        );
        assert_eq!(
            cells
                .iter()
                .find(|cell| cell.id.backend.as_deref() == Some("liteinst"))
                .unwrap()
                .timeout_seconds,
            20
        );
    }

    #[test]
    fn enabled_kvm_python_examples_keeps_its_measured_timeout() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let defaults: ManifestDefaults = serde_yaml::from_str(
            &fs::read_to_string(root.join("tests/e2e/manifests/defaults.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(defaults.timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
        assert_eq!(
            defaults.cpu_timeout_seconds,
            DEFAULT_TEST_CPU_TIMEOUT_SECONDS
        );

        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Enabled),
                test: Some("applications/kvm-python-examples".into()),
                mode: Some("verify".into()),
                backend: Some("kvm".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].timeout_seconds, 74);
        assert_eq!(cells[0].cpu_timeout_seconds, 25);
    }

    #[test]
    fn shipped_readdir_order_identity_retains_its_resolved_timeouts() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let cells = ManifestSet::load(&root)
            .unwrap()
            .select(&Selection {
                population: Some(Population::Required),
                test: Some("c-programs/readdir-order-identity".into()),
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(cells.len(), 1);
        let cell = &cells[0];
        assert_eq!(cell.timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
        assert_eq!(cell.cpu_timeout_seconds, DEFAULT_TEST_CPU_TIMEOUT_SECONDS);

        let multipliers = TimeoutMultipliers {
            cpu: 1.25,
            wall: 1.5,
        };
        let resolved = resolved_cell_timeouts(cell, multipliers).unwrap();
        assert_eq!(resolved.cpu_seconds, 28);
        assert_eq!(resolved.wall_seconds, 86);
        let mut context = run_context(&root);
        context.timeout_multipliers = multipliers;
        let retained = infrastructure_error_result(&context, cell, "fixture".into());
        assert_eq!(retained.timeout_seconds, resolved.wall_seconds);
        assert_eq!(
            retained.execution_cpu_timeout_seconds,
            Some(resolved.cpu_seconds)
        );
        assert_eq!(
            retained.execution_wall_timeout_seconds,
            Some(resolved.wall_seconds)
        );
    }

    #[test]
    fn shipped_plan_matches_the_frozen_timeout_calibration_partition() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let required = manifests
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        let enabled = manifests
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        // https://github.com/rrnewton/hermit/pull/2425 adds these two qualified
        // cells after the frozen calibration. Keep their identities explicit
        // without changing the historical population or timeout overrides.
        let regular_sink = required
            .iter()
            .filter(|cell| cell.id.test == "c-programs/record-replay-file-state-regular-sink")
            .map(|cell| (cell.id.mode.as_str(), cell.id.backend.as_deref()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            regular_sink,
            BTreeSet::from([("verify", Some("ptrace")), ("verify", Some("sabre"))])
        );
        // https://github.com/rrnewton/hermit/pull/3231 qualifies three new
        // cells without revising the frozen calibration or any timeout override.
        for population in [&required, &enabled] {
            let rng = population
                .iter()
                .filter(|cell| cell.id.test == "c-programs/random-readv-stream")
                .collect::<Vec<_>>();
            assert_eq!(rng.len(), 3);
            assert_eq!(
                rng.iter()
                    .map(|cell| (cell.id.mode.as_str(), cell.id.backend.as_deref()))
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    ("verify", Some("ptrace")),
                    ("verify", Some("kvm")),
                    ("replay", Some("ptrace")),
                ])
            );
            for cell in rng {
                assert_eq!(cell.cpu_timeout_seconds, DEFAULT_TEST_CPU_TIMEOUT_SECONDS);
                assert_eq!(cell.timeout_seconds, DEFAULT_TEST_WALL_TIMEOUT_SECONDS);
            }
        }
        assert_eq!(
            required.len(),
            CALIBRATED_CI_CELL_COUNT + KVM_RATCHET_CI_CELL_COUNT - KVM_RUN_1709_CI_REMOVAL_COUNT
                + KVM_PINNED_IMAGE_QUALIFIED_CI_CELL_COUNT
                + KVM_NEXT40_QUALIFIED_CI_CELL_COUNT
                + 2
                + KVM_2026_09_08_SELECTED_CI_CELL_COUNT
                + IPC_DETERMINISM_CHAOS_SELECTED_CI_CELL_COUNT
                + LITEINST_2026_09_16_SELECTED_CI_CELL_COUNT
                + LITEINST_2026_09_17_SELECTED_CI_CELL_COUNT
                + PTRACE_2026_09_24_SELECTED_CI_CELL_COUNT
                + 3 // the exact RNG identities asserted above
                + DBT_MATRIX_2026_09_29_SELECTED_CI_CELL_COUNT
                + STRICT_COMPAT_FOLD_2026_10_01_SELECTED_CI_CELL_COUNT
        );
        // Slice S13 of https://github.com/rrnewton/hermit/issues/3301 selected three
        // DBT verify cells that were enabled with ci:false and enabled one new
        // ci:false DBT verify cell; the timeouts module names them.
        let dbt_verify = |population: &[SelectedCell], test: &str| {
            population.iter().any(|cell| {
                cell.id.test == test
                    && cell.id.mode == "verify"
                    && cell.id.backend.as_deref() == Some("dbt")
            })
        };
        for test in DBT_MATRIX_2026_09_29_PROMOTED_CI_FALSE_TESTS {
            assert!(
                dbt_verify(&required, test),
                "{test} verify/dbt is not required"
            );
        }
        for test in DBT_MATRIX_2026_09_29_ENABLED_CI_FALSE_TESTS {
            assert!(
                dbt_verify(&enabled, test),
                "{test} verify/dbt is not enabled"
            );
            assert!(
                !dbt_verify(&required, test),
                "{test} verify/dbt is required"
            );
        }
        assert_eq!(
            enabled.len() - required.len(),
            NON_CI_CELL_COUNT
                - LITEINST_2026_09_16_SELECTED_CI_CELL_COUNT
                - DBT_MATRIX_2026_09_29_PROMOTED_CI_FALSE_TESTS.len()
                + DBT_MATRIX_2026_09_29_ENABLED_CI_FALSE_TESTS.len(),
            "the current manifest census records every enabled ci:false cell"
        );

        // The earlier group was already enabled; the later disabled group adds
        // equally to enabled and required, preserving enabled-minus-required.
        // Both groups retain their separate evidence and ordinary bounds.
        for calibration in LITEINST_2026_09_16_TIMEOUT_CALIBRATIONS
            .iter()
            .chain(&LITEINST_2026_09_17_TIMEOUT_CALIBRATIONS)
        {
            let cell = required
                .iter()
                .find(|cell| {
                    cell.id.test == calibration.test
                        && cell.id.mode == calibration.mode
                        && cell.id.backend.as_deref() == Some(calibration.backend)
                })
                .expect("a newly qualified LiteInst cell is missing from required selection");
            assert_eq!(cell.cpu_timeout_seconds, calibration.configured_cpu_seconds);
            assert_eq!(cell.timeout_seconds, calibration.configured_wall_seconds);
        }

        let observed = enabled
            .iter()
            .filter(|cell| {
                cell.cpu_timeout_seconds != DEFAULT_TEST_CPU_TIMEOUT_SECONDS
                    || cell.timeout_seconds != DEFAULT_TEST_WALL_TIMEOUT_SECONDS
            })
            .map(|cell| {
                (
                    (
                        cell.id.test.as_str(),
                        cell.id.mode.as_str(),
                        cell.id.backend.as_deref().unwrap_or("native"),
                    ),
                    (cell.cpu_timeout_seconds, cell.timeout_seconds),
                )
            })
            .collect::<BTreeMap<_, _>>();
        // The strict compatibility corpus (fold 1 of
        // https://github.com/rrnewton/hermit/issues/3448) keeps the wall bounds
        // its generated validation nodes carried, 60 s and 20 s for its five
        // bounded diagnostics, with CPU one second below each (a cell's CPU
        // budget must stay below its wall). These literals are the check; they
        // are not read back from compat.yaml.
        let (compat, observed): (BTreeMap<_, _>, BTreeMap<_, _>) = observed
            .into_iter()
            .partition(|((test, _, _), _)| test.starts_with("compat/"));
        assert_eq!(
            compat.len(),
            STRICT_COMPAT_FOLD_2026_10_01_SELECTED_CI_CELL_COUNT
        );
        for ((test, mode, backend), bounds) in &compat {
            assert_eq!((*mode, *backend), ("verify", "ptrace"), "{test}");
            let diagnostic = matches!(
                *test,
                "compat/df"
                    | "compat/ranlib"
                    | "compat/top"
                    | "compat/zstd"
                    | "compat/zstd-roundtrip"
            );
            assert_eq!(
                *bounds,
                if diagnostic { (19, 20) } else { (59, 60) },
                "{test}"
            );
        }
        let expected = EXPLICIT_TIMEOUT_CALIBRATIONS
            .iter()
            .chain(&KVM_RATCHET_TIMEOUT_CALIBRATIONS)
            .map(|calibration| {
                (
                    (calibration.test, calibration.mode, calibration.backend),
                    (
                        calibration.configured_cpu_seconds,
                        calibration.configured_wall_seconds,
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(observed, expected);
    }

    #[test]
    fn bucket_timeout_requires_a_reason_and_must_change_the_global_default() {
        let mut document = ManifestDocument {
            schema: MANIFEST_SCHEMA,
            bucket: "fixture".into(),
            timeout_seconds: Some(20),
            slow_reason: Some("measured bucket-wide work needs more than 15 seconds".into()),
            test: vec![recipe(true)],
        };
        document.slow_reason = None;
        assert_eq!(
            validate_document(&document, "fixture", Path::new("/"), 15).unwrap_err(),
            "fixture: bucket timeout_seconds requires a non-empty slow_reason"
        );
        document.slow_reason = Some("measured bucket-wide work needs more than 15 seconds".into());
        document.timeout_seconds = Some(15);
        assert_eq!(
            validate_document(&document, "fixture", Path::new("/"), 15).unwrap_err(),
            "fixture: bucket timeout_seconds redundantly repeats the global default"
        );
    }

    #[test]
    fn cell_timeout_requires_the_same_named_reason() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.timeout_seconds.insert("ptrace".into(), 30);
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 15).unwrap_err(),
            "fixture/test: verify timeout_seconds, cpu_timeout_seconds, and slow_reason must name the same backends"
        );
        mode.cpu_timeout_seconds.insert("ptrace".into(), 25);
        mode.slow_reason
            .insert("ptrace".into(), "measured above the inherited limit".into());
        assert!(validate_mode("fixture/test", "verify", &mode, 15).is_ok());
    }

    #[test]
    fn execution_wall_backstop_is_wider_than_the_cpu_budget() {
        let started = Instant::now();
        let deadline = execution_deadline_after_preparation(started, 57).unwrap();
        assert_eq!(
            remaining_cell_time_at(deadline, started),
            Duration::from_secs(57)
        );
        assert_eq!(
            remaining_cell_time_at(deadline, started + Duration::from_secs(57)),
            Duration::ZERO
        );
    }

    #[test]
    fn slow_preparation_does_not_reduce_the_execution_backstop() {
        let preparation_started = Instant::now();
        let prepared_at = preparation_started + Duration::from_millis(47_770);
        let execution_deadline = execution_deadline_after_preparation(prepared_at, 57).unwrap();
        assert_eq!(
            remaining_cell_time_at(execution_deadline, prepared_at),
            Duration::from_secs(57),
            "preparation must not consume the fresh explicit wall backstop"
        );
    }

    #[test]
    fn an_expired_wall_backstop_emits_typed_not_run_without_a_process() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prelaunch-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/prelaunch-timeout".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: root.clone(),
            env: BTreeMap::new(),
            argv: vec!["/bin/false".into()],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 15,
            verdict_path: Some(root.join("verdict.json")),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: root.clone(),
            expected_guest_exit: None,
            expected_stdout: None,
            expected_stdout_contains: None,
            comparator: Comparator::Strict,
            attempt: "1".into(),
            fixed_workdir_source: root.join("workdir/1"),
            normalize_ptrace_golden: false,
        };

        // This is the exact boundary reached when the aggregate execution
        // backstop is gone before a later attempt starts: no child process can
        // exist or have an exit status.
        let attempt =
            execute_spec_until(&spec, Instant::now(), 15, 57, None, &mut Vec::new()).unwrap();
        assert_eq!(attempt.outcome, "ERROR");
        assert_eq!(attempt.error_kind.as_deref(), Some("wall-timeout"));
        assert!(attempt.timed_out);
        assert_eq!((attempt.status, attempt.signal), (None, None));
        let report = attempt
            .verification_report
            .as_deref()
            .expect("pre-launch timeout must carry typed NotRun evidence");
        let report_sha256 = hex_digest(report.as_bytes());
        assert_eq!(
            attempt.verification_report_sha256.as_deref(),
            Some(report_sha256.as_str())
        );
        let report = VerificationReport::from_json_slice(report.as_bytes()).unwrap();
        assert_eq!(report.verdict, Verdict::NoResult);
        assert_eq!(
            report.no_result_reason,
            Some(crate::canonical_verdict::NoResultReason::NotRun)
        );
        assert_eq!(
            failure_class(
                &attempt.outcome,
                observed_result(
                    "verify",
                    &attempt.outcome,
                    std::slice::from_ref(&attempt),
                    attempt.error_kind.as_deref()
                ),
                attempt.error_kind.as_deref(),
            ),
            Some(FailureClass::NoResult)
        );
        let runtime = root.join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let mut cell = ptrace_cell("verify");
        let context = run_context(&root);
        // An unrelated recipe must never inspect the invalid lease transport.
        assert!(
            acquire_proc_locks_lease(
                &cell,
                Instant::now(),
                Some(OsStr::new("/missing/lease")),
                Some(OsStr::new("invalid")),
            )
            .unwrap()
            .is_none()
        );
        cell.test.program = Some("tests/c/proc_locks.c".into());
        let mut reached_guest = false;
        let failure = record_cell(&context, &cell, |_| {
            let _lease =
                acquire_proc_locks_lease(&cell, Instant::now(), Some(runtime.as_os_str()), None)?;
            reached_guest = true;
            unreachable!("expired lease deadline cannot reach guest launch")
        })
        .unwrap_err();
        assert!(!reached_guest);
        assert!(failure.attempts.is_empty());
        assert!(failure.observations.invocations.is_empty());
        let result = failure.into_result(&context, &cell);
        assert_eq!(result.outcome, "ERROR");
        assert_eq!(
            result.failure_class,
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );
        assert!(result.result.is_none());
        assert!(
            result
                .reason
                .as_deref()
                .unwrap()
                .contains("execution deadline before guest launch")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_exhausted_cpu_budget_is_typed_before_the_next_process_starts() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prelaunch-cpu-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "prelaunch-cpu-timeout", "/bin/false");
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(5),
            1,
            5,
            Some(0),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(attempt.outcome, "ERROR");
        assert_eq!(attempt.error_kind.as_deref(), Some("cpu-timeout"));
        assert!(attempt.timed_out);
        assert_eq!((attempt.status, attempt.signal), (None, None));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wait4_cpu_usage_moves_with_descendant_work() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-cpu-usage-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let measure = |label: &str, script: &str| {
            execute_process(
                ProcessRequest::new(
                    InvocationRole::Execution {
                        attempt_index: "1".into(),
                        backend: RequiredNullable::Null,
                    },
                    &root,
                    "/bin/sh",
                    &["-c".into(), script.into()],
                    &BTreeMap::new(),
                    &root.join(format!("{label}.stdout")),
                    &root.join(format!("{label}.stderr")),
                ),
                (Instant::now() + Duration::from_secs(10), None),
                &mut Vec::new(),
            )
            .unwrap()
        };
        let low = measure("low", ":");
        let high = measure("high", "head -c 134217728 /dev/zero | sha256sum >/dev/null");
        assert!(low.status.success());
        assert!(high.status.success());
        assert!(
            high.cpu_usage_usec > low.cpu_usage_usec.saturating_add(10_000),
            "adding 128 MiB of descendant hashing did not move CPU usage: low={} high={}",
            low.cpu_usage_usec,
            high.cpu_usage_usec
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn bounded_process(
        root: &Path,
        label: &str,
        script: &str,
        wall: Duration,
        cpu_budget_usec: u64,
    ) -> ProcessOutput {
        let mut observations = Vec::new();
        let output = execute_process(
            ProcessRequest::new(
                InvocationRole::Execution {
                    attempt_index: "1".into(),
                    backend: RequiredNullable::Null,
                },
                root,
                "/bin/sh",
                &["-c".into(), script.into()],
                &BTreeMap::new(),
                &root.join(format!("{label}.stdout")),
                &root.join(format!("{label}.stderr")),
            ),
            (Instant::now() + wall, Some(cpu_budget_usec)),
            &mut observations,
        )
        .unwrap();
        assert_process_observation(&observations[0], &output);
        output
    }

    #[test]
    fn live_cpu_accounting_excludes_an_unrelated_process_group() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-owned-cpu-accounting-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("3");
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        let reader = dagrun::proccpu::ProcessGroupCpu::new(pid).unwrap();
        // `stop_process_group` below owns the matching wait4; discard only the
        // std handle so the test exercises the production reaper.
        drop(child);
        let unrelated = bounded_process(
            &root,
            "unrelated",
            "while :; do :; done",
            Duration::from_millis(650),
            5_000_000,
        );
        assert_eq!(unrelated.timeout, Some(ProcessTimeout::Wall));
        let used = live_cpu_usage_usec(
            pid,
            reader
                .seconds()
                .expect("the launched sleeper must remain measurable"),
        )
        .unwrap();
        assert!(
            used < 100_000,
            "an idle process group unexpectedly included unrelated CPU: {used} usec"
        );
        stop_process_group(
            pid,
            Instant::now(),
            &mut FinalWaitObservation::NotApplicable,
        )
        .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn cpu_reader_test_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    fn spawn_cpu_reader_fixture(
        root: &Path,
        label: &str,
        script: &str,
        cpu_enabled: bool,
    ) -> (Child, Instant, InvocationCpuObservation) {
        let started = Instant::now();
        let request = ProcessRequest::new(
            InvocationRole::Execution {
                attempt_index: label.into(),
                backend: RequiredNullable::Null,
            },
            root,
            "/bin/sh",
            &["-c".into(), script.into()],
            &BTreeMap::new(),
            &root.join(format!("{label}.stdout")),
            &root.join(format!("{label}.stderr")),
        );
        let mut observation =
            InvocationCpuObservation::pending(1, request.role, request.command, cpu_enabled);
        let child = spawn_process(
            &request.cwd,
            &request.stdout,
            &request.stderr,
            &mut observation,
        )
        .unwrap();
        (child, started, observation)
    }

    fn owned_child_is_reaped(pid: u32) -> bool {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error();
        waited == -1 && error.raw_os_error() == Some(libc::ECHILD)
    }

    fn enabled_cpu(observation: &InvocationCpuObservation) -> &crate::cpu_evidence::LiveCpuEnabled {
        let LiveCpuObservation::Enabled(live) = &observation.live else {
            panic!("expected enabled live CPU observation: {observation:?}");
        };
        live
    }

    fn measured_final_cpu(observation: &InvocationCpuObservation) -> u64 {
        let LaunchObservation::Spawned { pid } = observation.launch else {
            panic!("expected a real launched process: {observation:?}");
        };
        let FinalWaitObservation::Reaped {
            pid: waited,
            cpu:
                WaitCpuObservation::Measured {
                    user_usec,
                    system_usec,
                    total_usec,
                },
            ..
        } = &observation.final_wait
        else {
            panic!("expected actual wait4 CPU and reap: {observation:?}");
        };
        assert_eq!(*waited, pid);
        assert_eq!(user_usec.checked_add(*system_usec), Some(*total_usec));
        assert!(owned_child_is_reaped(pid));
        *total_usec
    }

    fn validate_native_observation(observation: &InvocationCpuObservation) {
        CellCpuObservationsV1 {
            version: 1,
            binding: CellCpuBinding {
                run_id: "native-cpu-fixture".into(),
                hermit_sha: "0".repeat(40),
                lane: "portable".into(),
                category: "fixture".into(),
                test: "fixture/native-cpu".into(),
                mode: "naked".into(),
                backend: RequiredNullable::Null,
                outer_attempt: 1,
                run_index: RequiredNullable::Null,
            },
            invocations: vec![observation.clone()],
        }
        .validate()
        .unwrap();
    }

    fn assert_process_observation(observation: &InvocationCpuObservation, output: &ProcessOutput) {
        let final_cpu = measured_final_cpu(observation);
        let (cpu_usec, basis) = match &observation.returned_cpu_charge {
            ReturnedCpuCharge::Value { cpu_usec, basis } => (*cpu_usec, basis),
            ReturnedCpuCharge::Unavailable => panic!("successful monitor return lost CPU charge"),
        };
        assert_eq!(cpu_usec, output.cpu_usage_usec);
        match observation.termination {
            TerminationPath::CpuBudgetStop => {
                let live = enabled_cpu(observation);
                let RequiredNullable::Value(trigger) = &live.timeout_trigger else {
                    panic!("CPU stop must retain its actual triggering sample");
                };
                assert_eq!(basis, &ChargeBasis::MaxTriggerAndFinalWait4);
                assert_eq!(cpu_usec, final_cpu.max(trigger.cpu_usec));
                assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
            }
            TerminationPath::CompletedWait4
            | TerminationPath::FinalWaitCpuBudgetReturn
            | TerminationPath::WallBudgetStop => {
                assert_eq!(basis, &ChargeBasis::FinalWait4);
                assert_eq!(cpu_usec, final_cpu);
            }
            _ => panic!("unexpected successful monitor termination: {observation:?}"),
        }
        validate_native_observation(observation);
    }

    fn assert_cpu_source_roundtrip(row: &CellResult) {
        use crate::cpu_evidence::CellCpuHistoryV1;
        use crate::ledger::read_schema10_source_result;

        let source = read_schema10_source_result(&serde_json::to_vec(row).unwrap()).unwrap();
        let history = CellCpuHistoryV1::from_source_rows(&[(row.attempt, source.clone())])
            .unwrap()
            .unwrap();
        assert_eq!(
            history.attempts[0].observations(),
            row.cpu_observations.as_ref()
        );
        for pointer in [
            "/cpu_observations/binding/run_id",
            "/cpu_observations/invocations/0/live/polls",
            "/cpu_observations/invocations/0/final_wait/cpu/user_usec",
        ] {
            let mut invalid = source.clone();
            if let Some(value) = invalid.pointer_mut(pointer) {
                *value = JsonValue::String("tampered".into());
                assert!(
                    read_schema10_source_result(&serde_json::to_vec(&invalid).unwrap()).is_err(),
                    "{pointer}"
                );
            }
        }
    }

    #[test]
    fn owned_cpu_reader_lives_until_each_invocation_is_reaped() {
        use std::cell::Cell;
        use std::rc::Rc;

        // The monitor must bind the real reader once and keep it until the
        // child is reaped; that lifetime is what this test checks. Its samples
        // are synthetic so the counts below stay exact on any host. A real
        // sample is a census of the whole host /proc with a one-second scan
        // deadline, and it fails as a whole when a member of this fixture's
        // group (the shell reaps a `sleep` every 20 ms) is reaped while the
        // census pairs its pidfd with its stat file ("pidfd/stat generation
        // mismatch"); either failure is then served to every call for 500 ms
        // by proccpu's shared snapshot
        // (https://github.com/rrnewton/hermit/issues/3377). Real samples
        // through the monitor, with the monitor's one-second grace for
        // unavailable samples, are checked by
        // native_zombie_then_reaped_child_has_no_double_cpu_charge (every
        // sample, across several 500 ms snapshots) and
        // a_valid_cpu_sample_resets_the_unavailable_grace (one sample).
        struct ObservedReader {
            _reader: dagrun::proccpu::ProcessGroupCpu,
            pid: u32,
            registered: Instant,
            dropped_after_reap: Rc<Cell<bool>>,
        }
        impl Drop for ObservedReader {
            fn drop(&mut self) {
                self.dropped_after_reap.set(owned_child_is_reaped(self.pid));
            }
        }

        let root = cpu_reader_test_root("owned-reader-lifetime");
        let registered_invocations = Cell::new(0);
        for label in ["first", "second"] {
            let done = root.join(format!("{label}.done"));
            let (child, started, mut observation) = spawn_cpu_reader_fixture(
                &root,
                label,
                &format!("while [ ! -f {label}.done ]; do sleep 0.02; done"),
                true,
            );
            let pid = child.id();
            let registrations = Cell::new(0);
            let samples = Cell::new(0);
            let dropped_after_reap = Rc::new(Cell::new(false));
            let output = monitor_process(
                child,
                ProcessLimits {
                    deadline: Instant::now() + Duration::from_secs(5),
                    cpu_budget_usec: Some(5_000_000),
                    cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
                },
                started,
                &mut observation,
                |registered_pid| {
                    assert_eq!(registered_pid, pid);
                    registrations.set(registrations.get() + 1);
                    registered_invocations.set(registered_invocations.get() + 1);
                    dagrun::proccpu::ProcessGroupCpu::new(pid)
                        .map(|reader| ObservedReader {
                            _reader: reader,
                            pid,
                            registered: Instant::now(),
                            dropped_after_reap: Rc::clone(&dropped_after_reap),
                        })
                        .map_err(|error| error.to_string())
                },
                |reader| {
                    assert!(!dropped_after_reap.get());
                    samples.set(samples.get() + 1);
                    let seconds = samples.get() as f64 / 1000.0;
                    if samples.get() >= 3
                        && reader.registered.elapsed() >= Duration::from_millis(700)
                    {
                        fs::write(&done, b"complete").map_err(|error| error.to_string())?;
                    }
                    Ok(seconds)
                },
            )
            .unwrap();
            assert!(output.status.success());
            assert_eq!(output.timeout, None);
            assert_eq!(registrations.get(), 1);
            assert!(samples.get() >= 3);
            assert!(done.is_file(), "monitor must sample the reader for 700 ms");
            assert!(dropped_after_reap.get());
            assert!(owned_child_is_reaped(pid));
            assert_process_observation(&observation, &output);
            let live = enabled_cpu(&observation);
            assert_eq!(live.registration, RegistrationObservation::BoundOnce);
            assert_eq!(live.source_sample_calls, samples.get());
            assert_eq!(live.valid_polls, samples.get());
            assert_eq!(live.unavailable_polls, 0);
        }
        // Reaped invocations may legitimately reuse a numeric PID. Each still
        // constructs and drops its own reader through the factory above.
        assert_eq!(registered_invocations.get(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_cpu_registration_sampling_and_conversion_reap_the_owned_child() {
        use std::cell::Cell;

        let root = cpu_reader_test_root("unavailable-reader");
        for failure in ["registration", "sampling", "conversion"] {
            let (child, started, mut observation) =
                spawn_cpu_reader_fixture(&root, failure, "exec sleep 20", true);
            let pid = child.id();
            let registrations = Cell::new(0);
            let samples = Cell::new(0);
            let error = monitor_process(
                child,
                ProcessLimits {
                    deadline: started + Duration::from_secs(5),
                    cpu_budget_usec: Some(5_000_000),
                    cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
                },
                started,
                &mut observation,
                |registered_pid| {
                    assert_eq!(registered_pid, pid);
                    registrations.set(registrations.get() + 1);
                    if failure == "registration" {
                        Err("fixture registration unavailable".into())
                    } else {
                        dagrun::proccpu::ProcessGroupCpu::new(pid)
                            .map_err(|error| error.to_string())
                    }
                },
                |_| {
                    samples.set(samples.get() + 1);
                    if failure == "sampling" {
                        Err("fixture sample unavailable".into())
                    } else {
                        Ok(f64::NAN)
                    }
                },
            )
            .map(|_| ())
            .expect_err("unavailable accounting must refuse the command");
            assert!(started.elapsed() >= CELL_CPU_ACCOUNTING_GRACE);
            assert_eq!(
                registrations.get(),
                1,
                "never retry numeric-PID registration"
            );
            if failure == "registration" {
                assert_eq!(samples.get(), 0);
                assert!(error.contains("registration: fixture registration unavailable"));
            } else {
                assert!(samples.get() > 1);
                if failure == "sampling" {
                    assert!(error.contains("sampling: fixture sample unavailable"));
                } else {
                    assert!(error.contains("invalid live CPU seconds NaN"));
                }
            }
            assert!(error.contains("stopped and reaped its leader"), "{error}");
            assert!(owned_child_is_reaped(pid), "{failure}: {error}");
            measured_final_cpu(&observation);
            assert_eq!(
                observation.termination,
                TerminationPath::AccountingUnavailableStop
            );
            assert_eq!(
                observation.returned_cpu_charge,
                ReturnedCpuCharge::Unavailable
            );
            let live = enabled_cpu(&observation);
            assert_eq!(live.source_sample_calls, samples.get());
            assert_eq!(live.valid_polls, 0);
            assert!(live.unavailable_polls >= 2);
            let RequiredNullable::Value(last_error) = &live.last_error else {
                panic!("unavailability must retain its typed error");
            };
            assert_eq!(
                last_error.stage,
                match failure {
                    "registration" => CpuErrorStage::Registration,
                    "sampling" => CpuErrorStage::Sampling,
                    _ => CpuErrorStage::Conversion,
                }
            );
            validate_native_observation(&observation);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_or_cpu_disabled_commands_do_not_require_live_cpu_samples() {
        use std::cell::Cell;

        let root = cpu_reader_test_root("optional-reader");
        for cpu_budget_usec in [None, Some(5_000_000)] {
            let (child, started, mut observation) =
                spawn_cpu_reader_fixture(&root, "complete", "exit 0", cpu_budget_usec.is_some());
            let pid = child.id();
            let registrations = Cell::new(0);
            let output = monitor_process(
                child,
                ProcessLimits {
                    deadline: Instant::now() + Duration::from_secs(5),
                    cpu_budget_usec,
                    cpu_poll_interval: Duration::from_secs(5),
                },
                started,
                &mut observation,
                |_| -> Result<(), String> {
                    assert!(
                        cpu_budget_usec.is_some(),
                        "disabled accounting must not register"
                    );
                    registrations.set(registrations.get() + 1);
                    Err("fixture registration unavailable".into())
                },
                |_| panic!("wait4 completion must precede live sampling"),
            )
            .unwrap();
            assert!(output.status.success());
            assert_eq!(output.timeout, None);
            assert_eq!(registrations.get(), usize::from(cpu_budget_usec.is_some()));
            assert!(owned_child_is_reaped(pid));
            assert_process_observation(&observation, &output);
            if cpu_budget_usec.is_some() {
                let live = enabled_cpu(&observation);
                assert_eq!(live.polls, 0);
                assert_eq!(live.source_sample_calls, 0);
                assert!(matches!(
                    live.registration,
                    RegistrationObservation::Unavailable { .. }
                ));
            } else {
                assert_eq!(observation.live, LiveCpuObservation::Disabled);
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_valid_cpu_sample_resets_the_unavailable_grace() {
        let root = cpu_reader_test_root("reader-grace-reset");
        let done = root.join("done");
        let (child, started, mut observation) = spawn_cpu_reader_fixture(
            &root,
            "reset",
            "while [ ! -f done ]; do sleep 0.02; done",
            true,
        );
        let pid = child.id();
        let mut first_missing = None;
        let mut valid_samples = 0;
        let mut missing_after_valid = 0;
        let output = monitor_process(
            child,
            ProcessLimits {
                deadline: Instant::now() + Duration::from_secs(5),
                cpu_budget_usec: Some(5_000_000),
                cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
            },
            started,
            &mut observation,
            |pid| dagrun::proccpu::ProcessGroupCpu::new(pid).map_err(|error| error.to_string()),
            |reader| {
                let first = *first_missing.get_or_insert_with(Instant::now);
                if valid_samples == 0 && first.elapsed() >= Duration::from_millis(500) {
                    let seconds = reader.seconds().map_err(|error| error.to_string())?;
                    valid_samples += 1;
                    return Ok(seconds);
                }
                if valid_samples > 0 {
                    missing_after_valid += 1;
                    if first.elapsed() >= CELL_CPU_ACCOUNTING_GRACE + Duration::from_millis(200) {
                        fs::write(&done, b"complete").map_err(|error| error.to_string())?;
                    }
                }
                Err("fixture transient sample unavailable".into())
            },
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.timeout, None);
        assert_eq!(valid_samples, 1);
        assert!(missing_after_valid > 0);
        assert!(
            done.is_file(),
            "must survive beyond the original missing grace"
        );
        assert!(owned_child_is_reaped(pid));
        assert_process_observation(&observation, &output);
        let live = enabled_cpu(&observation);
        assert_eq!(live.valid_polls, 1);
        assert!(live.unavailable_polls > 1);
        assert!(matches!(live.last_error, RequiredNullable::Value(_)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_cpu_conversion_refuses_unrepresentable_values() {
        assert_eq!(live_cpu_usage_usec(42, 0.0), Ok(0));
        assert_eq!(live_cpu_usage_usec(42, 0.125), Ok(125_000));
        for seconds in [
            -0.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            u64::MAX as f64 / 1_000_000.0,
        ] {
            assert!(live_cpu_usage_usec(42, seconds).is_err(), "{seconds}");
        }
        // Converting the adjacent float to seconds and back rounds up to 2^64.
        let adjacent_usec = f64::from_bits((u64::MAX as f64).to_bits() - 1);
        assert!(live_cpu_usage_usec(42, adjacent_usec / 1_000_000.0).is_err());
        let representable_usec = f64::from_bits((u64::MAX as f64).to_bits() - 2);
        assert!(live_cpu_usage_usec(42, representable_usec / 1_000_000.0).is_ok());
    }

    fn bounded_spec(root: &Path, label: &str, script: &str) -> CellRunSpec {
        CellRunSpec {
            id: CellId {
                test: format!("fixture/{label}"),
                mode: "naked".into(),
                backend: None,
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: root.to_owned(),
            env: BTreeMap::new(),
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            guest_argv: vec![script.into()],
            timeout_seconds: 1,
            verdict_path: None,
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: root.join(label),
            expected_guest_exit: None,
            expected_stdout: None,
            expected_stdout_contains: None,
            comparator: Comparator::Strict,
            attempt: "1".into(),
            fixed_workdir_source: root.join(label).join("workdir/1"),
            normalize_ptrace_golden: false,
        }
    }

    fn native_cpu_cell(script: &str, runs: u64) -> SelectedCell {
        let mut cell = ptrace_cell("naked");
        cell.id.backend = None;
        cell.test.direct = Some(DirectCommand::Argv(vec![
            "/bin/sh".into(),
            "-c".into(),
            script.into(),
        ]));
        cell.test.modes.get_mut("naked").unwrap().runs = Some(runs);
        cell.timeout_seconds = 5;
        cell.cpu_timeout_seconds = 4;
        cell
    }

    #[test]
    fn actual_fd_reserve_refusal_preserves_unavailable_cpu_and_reap() {
        const CHILD: &str = "HERMIT_TEST_CPU_FD_RESERVE_CHILD";
        const CASE: &str =
            "runner::tests::actual_fd_reserve_refusal_preserves_unavailable_cpu_and_reap";
        if std::env::var_os(CHILD).is_none() {
            // setrlimit is process-wide: keep it out of the parallel libtest
            // process, and select only this case in the isolated child.
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", CASE, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            print!("{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        let root = cpu_reader_test_root("actual-fd-reserve");
        let context = run_context(&root);
        let cell = native_cpu_cell("exec sleep 20", 1);
        let mut limit = unsafe { std::mem::zeroed::<libc::rlimit>() };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        assert!(limit.rlim_cur >= 32);
        limit.rlim_cur = 32;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let started = Instant::now();
        let failure =
            run_cell(&context, &cell).expect_err("real descriptor reserve refusal must fail");
        assert!(started.elapsed() >= CELL_CPU_ACCOUNTING_GRACE, "{failure}");
        assert!(
            failure
                .to_string()
                .contains("registration: descriptor reserve"),
            "{failure}"
        );
        let row = failure.into_result(&context, &cell);
        assert_eq!(row.outcome, "ERROR");
        assert_eq!(row.cpu_usage_usec, None);
        assert!(row.attempts.is_empty());
        row.require_cpu_observations().unwrap();
        let observations = row.cpu_observations.as_ref().unwrap();
        assert_eq!(observations.invocations.len(), 1);
        let invocation = &observations.invocations[0];
        assert_eq!(
            invocation.termination,
            TerminationPath::AccountingUnavailableStop
        );
        assert_eq!(
            invocation.returned_cpu_charge,
            ReturnedCpuCharge::Unavailable
        );
        measured_final_cpu(invocation);
        let live = enabled_cpu(invocation);
        assert_eq!(
            live.registration,
            RegistrationObservation::Unavailable {
                reason: "descriptor reserve".into()
            }
        );
        assert_eq!(live.source_sample_calls, 0);
        assert_eq!(live.valid_polls, 0);
        assert!(live.unavailable_polls >= 2);
        assert_eq!(live.first, RequiredNullable::Null);
        assert_eq!(live.high_water, RequiredNullable::Null);
        assert_eq!(live.timeout_trigger, RequiredNullable::Null);
        let ledger = root.join("results.jsonl");
        append_result(&ledger, &row).unwrap();
        let bytes = fs::read(&ledger).unwrap();
        let decoded: CellResult = serde_json::from_slice(&bytes).unwrap();
        decoded.require_cpu_observations().unwrap();
        assert_cpu_source_roundtrip(&decoded);
        println!(
            "CPU_FD_REFUSAL_ROW {}",
            String::from_utf8(bytes).unwrap().trim()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_later_launch_retains_prior_attempt_and_cpu_observations() {
        let root = cpu_reader_test_root("prior-work-before-spawn-failure");
        let context = run_context(&root);
        let program = root.join("remove-self");
        fs::write(&program, "#!/bin/sh\nrm -- \"$0\"\nprintf measured\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let mut cell = native_cpu_cell("unused", 2);
        cell.test.direct = Some(DirectCommand::Argv(vec![program.display().to_string()]));
        let failure = run_cell(&context, &cell).expect_err("second launch must fail after unlink");
        let row = failure.into_result(&context, &cell);
        assert_eq!(row.outcome, "ERROR");
        assert_eq!(row.cpu_usage_usec, None);
        assert_eq!(row.attempts.len(), 1, "{row:?}");
        assert_eq!(row.attempts[0].stdout, "measured");
        let invocations = &row.cpu_observations.as_ref().unwrap().invocations;
        assert_eq!(invocations.len(), 2);
        assert_eq!(invocations[0].termination, TerminationPath::CompletedWait4);
        assert_eq!(
            row.attempts[0].cpu_usage_usec,
            Some(measured_final_cpu(&invocations[0]))
        );
        assert_eq!(invocations[1].ordinal, 2);
        assert!(matches!(
            invocations[1].launch,
            LaunchObservation::SpawnFailed {
                stage: SpawnStage::Spawn,
                ..
            }
        ));
        assert_eq!(
            invocations[1].final_wait,
            FinalWaitObservation::NotApplicable
        );
        assert_eq!(
            invocations[1].returned_cpu_charge,
            ReturnedCpuCharge::Unavailable
        );
        row.require_cpu_observations().unwrap();
        assert_cpu_source_roundtrip(&row);
        append_result(&root.join("results.jsonl"), &row).unwrap();
        println!(
            "CPU_PARTIAL_WORK_ROW {}",
            serde_json::to_string(&row).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_zombie_then_reaped_child_has_no_double_cpu_charge() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let root = cpu_reader_test_root("zombie-reap-cpu");
        let source = root.join("work.c");
        let program = root.join("work");
        // The parent holds the real zombie until the monitor releases it,
        // then holds the reaped state until a second real source sample.
        fs::write(&source, r#"
#define _GNU_SOURCE
#include <assert.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
static uint64_t cpu_ns(void) {
    struct timespec t;
    assert(clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t) == 0);
    return (uint64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}
static void phase(int fd, char mark) {
    assert(write(fd, &mark, 1) == 1);
    char ack;
    assert(read(fd, &ack, 1) == 1 && ack == mark);
}
int main(int argc, char **argv) {
    assert(argc == 2);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(fd >= 0);
    struct sockaddr_un address = { .sun_family = AF_UNIX };
    assert(strlen(argv[1]) < sizeof(address.sun_path));
    strcpy(address.sun_path, argv[1]);
    assert(connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
    int child_clock[2];
    assert(pipe(child_clock) == 0);
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        close(child_clock[0]);
        uint64_t started = cpu_ns();
        while (cpu_ns() - started < 250000000) {}
        uint64_t used = cpu_ns();
        assert(write(child_clock[1], &used, sizeof(used)) == sizeof(used));
        _exit(0);
    }
    close(child_clock[1]);
    uint64_t child_ns;
    assert(read(child_clock[0], &child_ns, sizeof(child_ns)) == sizeof(child_ns));
    siginfo_t info;
    assert(waitid(P_PID, child, &info, WEXITED | WNOWAIT) == 0);
    assert(info.si_pid == child && info.si_code == CLD_EXITED && info.si_status == 0);
    phase(fd, 'Z');
    int status;
    struct rusage used;
    assert(wait4(child, &status, 0, &used) == child);
    assert(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    phase(fd, 'R');
    printf("{\"child_clock_ns\":%llu,\"child_user_usec\":%llu,\"child_system_usec\":%llu,\"parent_clock_ns\":%llu}\n",
        (unsigned long long)child_ns,
        (unsigned long long)used.ru_utime.tv_sec * 1000000 + used.ru_utime.tv_usec,
        (unsigned long long)used.ru_stime.tv_sec * 1000000 + used.ru_stime.tv_usec,
        (unsigned long long)cpu_ns());
    return 0;
}
"#).unwrap();
        assert!(
            Command::new("cc")
                .args(["-O2", "-Wall", "-Werror"])
                .arg(&source)
                .arg("-o")
                .arg(&program)
                .status()
                .unwrap()
                .success()
        );
        let socket = root.join("phase.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let request = ProcessRequest::new(
            InvocationRole::Execution {
                attempt_index: "1".into(),
                backend: RequiredNullable::Null,
            },
            &root,
            program.to_str().unwrap(),
            &[socket.display().to_string()],
            &BTreeMap::new(),
            &root.join("work.stdout"),
            &root.join("work.stderr"),
        );
        let started = Instant::now();
        let mut observation =
            InvocationCpuObservation::pending(1, request.role, request.command, true);
        let child = spawn_process(
            &request.cwd,
            &request.stdout,
            &request.stderr,
            &mut observation,
        )
        .unwrap();
        let mut connection = None;
        let mut phase = None;
        let mut phase_samples = Vec::new();
        let output = monitor_process(
            child,
            ProcessLimits {
                deadline: started + Duration::from_secs(5),
                cpu_budget_usec: Some(2_000_000),
                cpu_poll_interval: CELL_CPU_POLL_INTERVAL,
            },
            started,
            &mut observation,
            |pid| dagrun::proccpu::ProcessGroupCpu::new(pid).map_err(|error| error.to_string()),
            |reader| {
                // Always call the actual generation-bound source first. The
                // handshake never substitutes a measured value or adds polls.
                let seconds = reader.seconds().map_err(|error| error.to_string())?;
                if connection.is_none() {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(true).map_err(|e| e.to_string())?;
                            connection = Some(stream);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) => return Err(error.to_string()),
                    }
                }
                if let Some(stream) = &mut connection {
                    if phase.is_none() {
                        let mut mark = [0];
                        match stream.read(&mut mark) {
                            Ok(1) => phase = Some((mark[0], Instant::now())),
                            Ok(_) => return Err("native phase peer closed early".into()),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(error) => return Err(error.to_string()),
                        }
                    }
                    // AU's existing shared snapshot TTL is 500ms. Hold each
                    // state longer so its accepted sample cannot predate it.
                    if let Some((mark, _)) = phase.filter(|(_, since): &(u8, Instant)| {
                        since.elapsed() >= Duration::from_millis(600)
                    }) {
                        phase_samples.push((mark, live_cpu_usage_usec(0, seconds)?));
                        stream.write_all(&[mark]).map_err(|e| e.to_string())?;
                        phase = None;
                    }
                }
                Ok(seconds)
            },
        )
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            fs::read_to_string(&request.stderr).unwrap()
        );
        assert_eq!(output.timeout, None);
        assert_process_observation(&observation, &output);
        assert_eq!(
            phase_samples
                .iter()
                .map(|(mark, _)| *mark)
                .collect::<Vec<_>>(),
            b"ZR"
        );
        let clocks: JsonValue =
            serde_json::from_slice(&fs::read(&request.stdout).unwrap()).unwrap();
        assert!(clocks["child_clock_ns"].as_u64().unwrap() >= 250_000_000);
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        assert!(ticks > 0);
        // Two processes, each with at most user/system tick quantization;
        // wait4's two microsecond components contribute at most 2us rounding.
        // This bound derives from the measurement units, not observed runs.
        let quantization_usec = 4 * 1_000_000u64.div_ceil(ticks as u64) + 2;
        let final_cpu = measured_final_cpu(&observation);
        let child_cpu = clocks["child_user_usec"].as_u64().unwrap()
            + clocks["child_system_usec"].as_u64().unwrap();
        assert!(child_cpu > quantization_usec * 2);
        assert!(final_cpu >= child_cpu);
        let RequiredNullable::Value(high) = &enabled_cpu(&observation).high_water else {
            panic!("native control must retain real live CPU samples");
        };
        assert!(
            high.cpu_usec <= final_cpu + quantization_usec,
            "live high-water duplicated waited CPU: {observation:?}"
        );
        for (_, sample) in &phase_samples {
            assert!(
                *sample + quantization_usec >= child_cpu,
                "child CPU disappeared across reap"
            );
        }
        println!(
            "CPU_NATIVE_ZOMBIE_REAP {}",
            serde_json::json!({
                "observation": observation, "clocks": clocks,
                "phase_samples": phase_samples, "clock_ticks_per_second": ticks,
                "quantization_bound_usec": quantization_usec,
            })
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cpu_burner_is_stopped_by_the_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-live-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "burner", "while :; do :; done");
        let mut observations = Vec::new();
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(5),
            1,
            5,
            Some(1_000_000),
            &mut observations,
        )
        .unwrap();
        assert!(attempt.timed_out);
        assert_eq!(attempt.error_kind.as_deref(), Some("cpu-timeout"));
        assert_eq!(attempt.reason.as_deref(), Some("cell exceeded 1 CPU s"));
        assert!(attempt.cpu_usage_usec.unwrap() >= 1_000_000);
        assert_eq!(
            serde_json::to_value(&attempt).unwrap()["error_kind"],
            "cpu-timeout",
            "the retained evidence must distinguish a CPU timeout from wall timeout"
        );
        let observation = &observations[0];
        assert_eq!(observation.termination, TerminationPath::CpuBudgetStop);
        assert_process_observation(
            observation,
            &ProcessOutput {
                status: ExitStatus::from_raw(match observation.final_wait {
                    FinalWaitObservation::Reaped { raw_status, .. } => raw_status,
                    _ => panic!("missing reap"),
                }),
                timeout: Some(ProcessTimeout::Cpu),
                cpu_usage_usec: attempt.cpu_usage_usec.unwrap(),
            },
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_process_is_checked_against_cpu_budget_between_polls() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-completed-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut observations = Vec::new();
        let output = execute_process_with_cpu_poll_interval(
            ProcessRequest::new(
                InvocationRole::Execution {
                    attempt_index: "1".into(),
                    backend: RequiredNullable::Null,
                },
                &root,
                "/bin/sh",
                &[
                    "-c".into(),
                    "head -c 8388608 /dev/zero | sha256sum >/dev/null".into(),
                ],
                &BTreeMap::new(),
                &root.join("completed.stdout"),
                &root.join("completed.stderr"),
            ),
            ProcessLimits {
                deadline: Instant::now() + Duration::from_secs(5),
                cpu_budget_usec: Some(1),
                cpu_poll_interval: Duration::from_secs(5),
            },
            &mut observations,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
        assert!(output.cpu_usage_usec >= 1);
        assert_process_observation(&observations[0], &output);
        assert_eq!(
            observations[0].termination,
            TerminationPath::FinalWaitCpuBudgetReturn
        );
        assert_eq!(enabled_cpu(&observations[0]).polls, 0);
        assert_eq!(
            enabled_cpu(&observations[0]).timeout_trigger,
            RequiredNullable::Null
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sleeping_process_is_stopped_by_the_wall_backstop() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-live-wall-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spec = bounded_spec(&root, "sleeper", "sleep 5");
        let mut observations = Vec::new();
        let attempt = execute_spec_until(
            &spec,
            Instant::now() + Duration::from_secs(1),
            1,
            1,
            Some(5_000_000),
            &mut observations,
        )
        .unwrap();
        assert!(attempt.timed_out);
        assert_eq!(attempt.error_kind.as_deref(), Some("wall-timeout"));
        assert_eq!(
            attempt.reason.as_deref(),
            Some("cell exceeded 1 wall s backstop (1 s CPU budget)")
        );
        assert!(attempt.cpu_usage_usec.unwrap() < 5_000_000);
        assert_eq!(
            serde_json::to_value(&attempt).unwrap()["error_kind"],
            "wall-timeout",
            "the retained evidence must distinguish a wall timeout from CPU timeout"
        );
        let observation = &observations[0];
        assert_eq!(observation.termination, TerminationPath::WallBudgetStop);
        assert_process_observation(
            observation,
            &ProcessOutput {
                status: ExitStatus::from_raw(match observation.final_wait {
                    FinalWaitObservation::Reaped { raw_status, .. } => raw_status,
                    _ => panic!("missing reap"),
                }),
                timeout: Some(ProcessTimeout::Wall),
                cpu_usage_usec: attempt.cpu_usage_usec.unwrap(),
            },
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn descheduled_process_may_exceed_the_old_wall_bound_and_pass() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-descheduled-process-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let started = Instant::now();
        let output = bounded_process(
            &root,
            "stopped",
            "(sleep 0.3; kill -CONT $$) & kill -STOP $$; printf resumed",
            Duration::from_secs(2),
            200_000,
        );
        assert!(output.status.success());
        assert_eq!(output.timeout, None);
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "the process did not remain descheduled long enough to exercise wall-vs-CPU"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_cpu_budget_counts_descendant_work() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-descendant-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let output = bounded_process(
            &root,
            "descendant",
            "sh -c 'while :; do :; done' & wait",
            Duration::from_secs(5),
            100_000,
        );
        assert_eq!(output.timeout, Some(ProcessTimeout::Cpu));
        assert!(output.cpu_usage_usec >= 100_000);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_processes_share_one_aggregate_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-aggregate-cpu-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let limit = 300_000u64;
        let first = bounded_process(
            &root,
            "first",
            "head -c 8388608 /dev/zero | sha256sum >/dev/null",
            Duration::from_secs(5),
            limit,
        );
        assert!(first.status.success());
        assert_eq!(first.timeout, None);
        assert!(first.cpu_usage_usec < limit);
        let second = bounded_process(
            &root,
            "second",
            "while :; do :; done",
            Duration::from_secs(5),
            limit - first.cpu_usage_usec,
        );
        assert_eq!(second.timeout, Some(ProcessTimeout::Cpu));
        assert!(
            first.cpu_usage_usec.saturating_add(second.cpu_usage_usec) >= limit,
            "second process did not consume the remainder of the shared CPU budget"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cpu_usage_aggregation_refuses_missing_or_overflowing_measurements() {
        assert_eq!(checked_add_cpu_usage(Some(2), Some(3)), Some(5));
        assert_eq!(checked_add_cpu_usage(Some(2), None), None);
        assert_eq!(checked_add_cpu_usage(None, Some(3)), None);
        assert_eq!(checked_add_cpu_usage(Some(u64::MAX), Some(1)), None);
    }

    #[test]
    fn ptrace_golden_normalization_is_bounded_and_accounts_a_timeout() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-normalization-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("run1_log_fixture.log"), b"retained log\n").unwrap();
        let program = root.join("hermit");
        fs::write(&program, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();

        let normalized = normalize_ptrace_golden(
            program.to_str().unwrap(),
            &root,
            &root,
            Instant::now() + Duration::from_secs(5),
            Some(200_000),
            (1, 5),
            (
                &mut Vec::new(),
                InvocationRole::PtraceNormalization {
                    execution_ordinal: 1,
                },
            ),
        );
        let error = normalized
            .result
            .expect_err("normalization must obey the shared CPU budget");
        assert!(error.contains("CPU"), "{error}");
        assert!(
            normalized
                .cpu_usage_usec
                .is_some_and(|usage| usage >= 100_000),
            "timed-out normalization must retain its measured CPU: {:?}",
            normalized.cpu_usage_usec
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sleeping_repeated_invocations_do_not_consume_the_cpu_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-cell-deadline-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let mut test = recipe(true);
        let counter = root.join("counter");
        test.direct = Some(DirectCommand::Argv(vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "count=$(cat '{}' 2>/dev/null || printf 0); count=$((count + 1)); printf '%s' \"$count\" > '{}'; sleep 0.7; printf '%s\\n' \"$count\"",
                counter.display(),
                counter.display()
            ),
        ]));
        let mut mode = test.modes.remove("verify").unwrap();
        mode.runs = Some(3);
        test.modes.insert("naked".into(), mode);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test,
            enabled: true,
            timeout_seconds: 3,
            cpu_timeout_seconds: 1,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("missing-hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };

        let result = run_cell(&context, &cell).unwrap();
        assert_eq!(result.attempts.len(), 3);
        let observations = result
            .cpu_observations
            .as_ref()
            .expect("real cell invocations must retain their CPU observations");
        assert_eq!(observations.invocations.len(), 3);
        result.require_cpu_observations().unwrap();
        assert!(result.attempts.iter().all(|attempt| !attempt.timed_out));
        assert_eq!(result.result, Some(ObservedResult::Pass));
        assert_eq!(result.failure_class, None);
        let duration_ms = result
            .duration_ms
            .expect("a cell that executed must report measured wall time");
        assert!(
            (2_000..3_000).contains(&duration_ms),
            "three sleeping attempts should pass despite exceeding the old one-second wall cap: {duration_ms}ms"
        );
        let attempt_cpu_usage_usec = result.attempts.iter().try_fold(0u64, |total, attempt| {
            checked_add_cpu_usage(Some(total), attempt.cpu_usage_usec)
        });
        assert_eq!(
            result.cpu_usage_usec, attempt_cpu_usage_usec,
            "the cell CPU figure must sum every process-owned attempt measurement"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_inapplicable_is_a_typed_nonpass_and_a_junit_skip() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-host-inapplicable-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut test = recipe(true);
        test.requires = vec!["cpuid".into()];
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let result = host_inapplicable_result(
            &context,
            &cell,
            "NOT RUN, NOT a pass, no coverage: planted absence".into(),
        );
        assert_eq!(result.outcome, "HOST-INAPPLICABLE");
        assert_eq!(result.result, None);
        assert_eq!(
            result.failure_class,
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        );
        assert!(result.attempts.is_empty());
        assert!(result.binary_sha256.is_none());
        assert_eq!(result.error_kind, None);
        assert_eq!(result.duration_ms, None);
        assert_eq!(result.cpu_usage_usec, None);
        let results = root.join("results.jsonl");
        append_result(&results, &result).unwrap();
        assert!(
            !results.exists(),
            "a cell withheld for an unmet prerequisite must not publish a cell row"
        );
        let row = serde_json::to_value(&result).unwrap();
        assert!(
            row.get("duration_ms").is_none(),
            "a cell that never executed must not publish a measured zero wall time"
        );
        assert!(
            row["cpu_usage_usec"].is_null(),
            "a cell that never executed must publish null rather than a measured zero CPU time"
        );
        let mut measured_zero = result.clone();
        measured_zero.duration_ms = Some(0);
        assert_eq!(
            serde_json::to_value(measured_zero).unwrap()["duration_ms"],
            0,
            "a measured sub-millisecond duration must remain zero"
        );

        let junit = root.join("junit.xml");
        write_junit(&junit, &[result]).unwrap();
        let xml = fs::read_to_string(&junit).unwrap();
        assert!(xml.contains("tests=\"1\" failures=\"0\" errors=\"0\" skipped=\"1\""));
        assert!(
            xml.contains(
                "<skipped message=\"NOT RUN, NOT a pass, no coverage: planted absence\"/>"
            )
        );
        assert!(!xml.contains(" time=\"0.000\""));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn junit_names_absent_failure_reasons_instead_of_guessing() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-junit-absent-reason-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let mut failed = cell_result_that_located_nothing();
        failed.outcome = "FAIL".into();
        failed.reason = None;
        let mut errored = cell_result_that_located_nothing();
        errored.outcome = "ERROR".into();
        errored.reason = None;
        let junit = root.join("junit.xml");
        write_junit(&junit, &[failed, errored]).unwrap();
        let xml = fs::read_to_string(&junit).unwrap();
        assert!(
            xml.contains("<failure>no specific failure reason was recorded</failure>"),
            "{xml}"
        );
        assert!(
            xml.contains("<error>no specific error reason was recorded</error>"),
            "{xml}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn retry_preserves_a_divergence_when_the_later_row_passes() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-durable-row-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let mut context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let path = root.join("results.jsonl");
        prepare_result_path(&path).unwrap();
        let mut first = infrastructure_error_result(&context, &cell, "forced failure".into());
        first.outcome = "FAIL".into();
        first.result = Some(ObservedResult::DeterminismFailure);
        first.failure_class = Some(FailureClass::ProductFailure);
        first.error_kind = None;
        assert_eq!(first.duration_ms, None);
        first.duration_ms = Some(111);
        first.first_divergent_record = Some(93);
        first.first_divergent_syscall = Some(37);
        first.first_divergent_scheduler_turn = Some(68);
        first.first_divergent_virtual_nanoseconds = Some(7);
        first.first_divergent_left_message = Some("INFO detcore: left event".into());
        first.first_divergent_right_message = Some("INFO detcore: right event".into());
        append_result(&path, &first).unwrap();

        // A validate retry starts a fresh harness process and prepares the same
        // path again. This call is the negative-control boundary: replacing it
        // with the former fs::write(path, b"") drops the first observation.
        prepare_result_path(&path).unwrap();
        context.attempt = 2;
        let mut second = infrastructure_error_result(&context, &cell, "forced retry".into());
        second.outcome = "PASS".into();
        second.result = Some(ObservedResult::Pass);
        second.failure_class = None;
        second.error_kind = None;
        second.duration_ms = Some(222);
        append_result(&path, &second).unwrap();

        let rows = fs::read_to_string(&path).unwrap();
        let rows = rows
            .lines()
            .map(|line| serde_json::from_str::<JsonValue>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["attempt"], 1);
        assert_eq!(rows[0]["duration_ms"], 111);
        assert_eq!(rows[0]["timeout_seconds"], 15);
        assert_eq!(rows[0]["outcome"], "FAIL");
        assert_eq!(rows[0]["result"], "determinism-failure");
        assert_eq!(rows[0]["failure_class"], "product_failure");
        assert_eq!(rows[0]["first_divergent_record"], 93);
        assert_eq!(rows[0]["first_divergent_syscall"], 37);
        assert_eq!(rows[0]["first_divergent_scheduler_turn"], 68);
        assert_eq!(rows[0]["first_divergent_virtual_nanoseconds"], 7);
        assert_eq!(
            rows[0]["first_divergent_left_message"],
            "INFO detcore: left event"
        );
        assert_eq!(
            rows[0]["first_divergent_right_message"],
            "INFO detcore: right event"
        );
        assert_eq!(rows[1]["attempt"], 2);
        assert_eq!(rows[1]["duration_ms"], 222);
        assert_eq!(rows[1]["timeout_seconds"], 15);
        assert_eq!(rows[1]["outcome"], "PASS");
        assert_eq!(rows[1]["result"], "pass");
        assert_eq!(rows[1]["failure_class"], JsonValue::Null);
        assert!(rows[1]["first_divergent_left_message"].is_null());
        assert!(rows[1]["first_divergent_right_message"].is_null());
        assert_ne!(rows[0]["artifact_dir"], rows[1]["artifact_dir"]);
        assert!(
            rows[1]["artifact_dir"]
                .as_str()
                .unwrap()
                .ends_with("-attempt-2")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_preserves_changed_product_result_and_attribution() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-retry-result-class-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("results.jsonl");
        prepare_result_path(&path).unwrap();

        let mut first = cell_result_that_located_nothing();
        first.attempt = 1;
        first.outcome = "FAIL".into();
        first.result = Some(ObservedResult::DeterminismFailure);
        first.failure_class = Some(FailureClass::ProductFailure);
        first.error_kind = None;
        first.artifact_dir = "/repo/artifacts-attempt-1".into();
        append_result(&path, &first).unwrap();

        let mut second = first.clone();
        second.attempt = 2;
        second.result = Some(ObservedResult::CrashError);
        second.artifact_dir = "/repo/artifacts-attempt-2".into();
        append_result(&path, &second).unwrap();

        let rows = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(rows[1].result, Some(ObservedResult::CrashError));
        assert_eq!(rows[0].failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(rows[1].failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(rows[0].error_kind, None);
        assert_eq!(rows[1].error_kind, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn required_selection_is_per_backend_while_enabled_selection_keeps_the_failure_visible() {
        let mut test = recipe(false);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.backends_enabled = vec!["ptrace".into(), "liteinst".into()];
        mode.backends_disabled.remove("liteinst");
        mode.ci = CiSelectionSpec::PerBackend(BTreeMap::from([
            ("ptrace".into(), true),
            ("liteinst".into(), false),
        ]));
        mode.ci_disabled_reason = Some(CiDisabledReasonSpec::PerBackend(BTreeMap::from([(
            "liteinst".into(),
            BackendCiDisabledReason {
                result: CiDisabledResult::DeterminismFailure,
                evidence: "ignored/results/liteinst.jsonl".into(),
                reason: "canonical comparison diverged at scheduler turn 10".into(),
            },
        )])));
        validate_mode("fixture/test", "verify", mode, 15).unwrap();
        let set = ManifestSet {
            documents: Vec::new(),
            tests: BTreeMap::from([(
                test.id.clone(),
                ("fixture".into(), 15, DEFAULT_TEST_CPU_TIMEOUT_SECONDS, test),
            )]),
        };
        let required = set
            .select(&Selection {
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].id.backend.as_deref(), Some("ptrace"));
        let enabled = set
            .select(&Selection {
                population: Some(Population::Enabled),
                ..Selection::default()
            })
            .unwrap();
        assert_eq!(enabled.len(), 2);
        assert!(
            enabled
                .iter()
                .any(|cell| cell.id.backend.as_deref() == Some("liteinst"))
        );
    }

    #[test]
    fn yaml_parses_structured_per_backend_selection() {
        let mode: ModeRecipe = serde_yaml::from_str(
            r#"
ci:
  ptrace: true
  liteinst: false
workdir: /tmp
ci_disabled_reason:
  liteinst:
    result: determinism-failure
    evidence: ignored/results/liteinst.jsonl
    reason: canonical comparison diverged at scheduler turn 10
backends_enabled: [ptrace, liteinst]
backends_disabled:
  dbt: unsupported
  kvm: unsupported
  sabre: unsupported
"#,
        )
        .unwrap();
        validate_mode("fixture/test", "verify", &mode, 15).unwrap();
        assert_eq!(mode.workdir.as_deref(), Some("/tmp"));
        let policy = ci_selection(&mode).unwrap();
        assert!(policy.selected("ptrace"));
        assert!(!policy.selected("liteinst"));
        assert_eq!(
            policy.reason("liteinst").unwrap().result,
            Some(CiDisabledResult::DeterminismFailure)
        );
    }

    #[test]
    fn rejects_relative_run_workdir() {
        let mut mode = recipe(true).modes.remove("verify").unwrap();
        mode.workdir = Some("tmp".into());
        assert_eq!(
            validate_mode("fixture/test", "verify", &mode, 15).unwrap_err(),
            "fixture/test: verify workdir must be an absolute path"
        );
    }

    #[test]
    fn rejects_workdir_for_non_run_modes() {
        for mode in ["replay", "naked"] {
            assert_eq!(
                validate_mode_workdir("fixture/test", mode, Some("/tmp"), &[]).unwrap_err(),
                format!("fixture/test: {mode} workdir is supported only by Hermit run modes")
            );
        }
    }

    #[test]
    fn workdir_accepts_every_hermit_run_backend() {
        let ptrace = vec!["ptrace".into()];
        assert_eq!(
            validate_mode_workdir("fixture/test", "verify", Some("/tmp"), &ptrace),
            Ok(())
        );

        for mode in ["verify", "chaos", "custom"] {
            for backends in [vec!["dbt".into()], vec!["ptrace".into(), "dbt".into()]] {
                assert_eq!(
                    validate_mode_workdir("fixture/test", mode, Some("/tmp"), &backends),
                    Ok(())
                );
            }
        }
    }

    /// `--backend` is a global Hermit option: `hermit run --backend X` is refused
    /// with a usage error. Every hermit invocation the runner builds must
    /// therefore carry the backend before the `run` subcommand, and never after.
    #[test]
    fn hermit_backend_is_passed_before_the_run_subcommand() {
        for (mode, seed) in [("verify", None), ("chaos", Some(7)), ("custom", None)] {
            let mut test = recipe(true);
            let recipe_mode = test.modes["verify"].clone();
            test.modes = BTreeMap::from([(mode.to_string(), recipe_mode)]);
            let cell = SelectedCell {
                category: "fixture".into(),
                id: CellId {
                    test: test.id.clone(),
                    mode: mode.into(),
                    backend: Some("ptrace".into()),
                },
                test,
                enabled: true,
                timeout_seconds: 15,
                cpu_timeout_seconds: 10,
            };
            let context = RunContext {
                root: PathBuf::from("/repo"),
                hermit_bin: PathBuf::from("/repo/hermit"),
                result_root: PathBuf::from("/repo/results"),
                build_root: PathBuf::from("/repo/build"),
                run_id: "fixture".into(),
                machine_shortname: "fixture-host".into(),
                kernel_version: "7.1.3-fixture".into(),
                host_capabilities: fixture_host_capabilities(),
                attempt: 1,
                run_index: None,
                epoch: "2026-01-01T00:00:00Z".into(),
                source_sha: "0".repeat(40),
                binary_build_sha: None,
                source_dirty: false,
                prebuilt: false,
                keep_logs: false,
                parity_retained: BTreeSet::new(),
                run_verify_strict: true,
                record_verify_strict: true,
                timeout_multipliers: TimeoutMultipliers::default(),
                scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
                isolated_workdir: None,
            };
            let spec = build_spec(
                &context,
                &cell,
                PathBuf::from("/repo/results/cell"),
                vec!["/bin/true".into()],
                "1",
                seed,
                cell.timeout_seconds,
            )
            .unwrap_or_else(|error| panic!("{mode}: {error}"));
            let run = spec
                .argv
                .iter()
                .position(|arg| arg == "run")
                .unwrap_or_else(|| panic!("{mode}: no run subcommand in {:?}", spec.argv));
            let backend = spec
                .argv
                .iter()
                .position(|arg| arg == "--backend")
                .unwrap_or_else(|| panic!("{mode}: no --backend in {:?}", spec.argv));
            assert!(
                backend < run,
                "{mode}: --backend after run: {:?}",
                spec.argv
            );
            assert_eq!(spec.argv[backend + 1], "ptrace", "{mode}: {:?}", spec.argv);
            assert!(
                !spec.argv[run..]
                    .iter()
                    .any(|arg| arg == "--backend" || arg.starts_with("--backend=")),
                "{mode}: a subcommand-level --backend in {:?}",
                spec.argv
            );
        }
    }

    #[test]
    fn run_workdir_precedes_the_guest_separator() {
        let mut test = recipe(true);
        test.modes.get_mut("verify").unwrap().workdir = Some("/tmp".into());
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let workdir = spec
            .argv
            .windows(2)
            .position(|args| args == ["--workdir", "/tmp"])
            .unwrap();
        assert!(workdir < separator);
    }

    #[test]
    fn hermetic_spec_mounts_a_fresh_test_root_and_limits_guest_environment() {
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(7),
            isolated_workdir: Some(PathBuf::from("/test")),
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let mount = spec
            .argv
            .iter()
            .position(|arg| arg == "--mount=type=tmpfs,target=/test")
            .unwrap();
        let workdir = spec
            .argv
            .windows(2)
            .position(|args| args == ["--workdir", "/test"])
            .unwrap();
        assert!(mount < separator && workdir < separator);
        assert_equalized_guest_env(&spec.argv, "/repo/results/cell", "/test", "7");

        let mut replay_test = recipe(true);
        let replay_mode = replay_test.modes.remove("verify").unwrap();
        replay_test.modes.insert("replay".into(), replay_mode);
        let replay_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: replay_test.id.clone(),
                mode: "replay".into(),
                backend: Some("ptrace".into()),
            },
            test: replay_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay"),
            vec!["/bin/true".into()],
            "1",
            None,
            7,
        )
        .unwrap();
        assert!(
            replay
                .argv
                .windows(2)
                .any(|window| { window[0] == "--record-timeout" && window[1] == "7" })
        );
        assert_eq!(replay.timeout_seconds, 7);
        assert!(replay.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert!(
            replay
                .argv
                .iter()
                .any(|arg| arg == "--mount=type=tmpfs,target=/test")
        );
        assert_minimal_guest_env(&replay.argv, "/repo/results/replay", "/test", "7");

        let mut custom_test = recipe(true);
        let mut custom_mode = custom_test.modes.remove("verify").unwrap();
        custom_mode.args = vec!["--base-env".into(), "minimal".into()];
        custom_test.modes.insert("custom".into(), custom_mode);
        let custom_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: custom_test.id.clone(),
                mode: "custom".into(),
                backend: Some("ptrace".into()),
            },
            test: custom_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let custom = build_spec(
            &context,
            &custom_cell,
            PathBuf::from("/repo/results/custom"),
            vec!["/bin/true".into()],
            "1",
            None,
            custom_cell.timeout_seconds,
        )
        .unwrap();
        assert!(
            custom
                .argv
                .windows(2)
                .any(|args| args == ["--base-env", "minimal"])
        );
        assert!(
            custom
                .argv
                .iter()
                .any(|arg| arg == "--mount=type=tmpfs,target=/test")
        );
        assert_minimal_guest_env(&custom.argv, "/repo/results/custom", "/test", "7");
        let mut naked_test = recipe(true);
        let naked_mode = naked_test.modes.remove("verify").unwrap();
        naked_test.modes.insert("naked".into(), naked_mode);
        let naked_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: naked_test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test: naked_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let naked_error = build_spec(
            &context,
            &naked_cell,
            PathBuf::from("/repo/results/naked"),
            vec!["/bin/true".into()],
            "1",
            None,
            naked_cell.timeout_seconds,
        )
        .unwrap_err();
        assert_eq!(
            naked_error,
            "hermetic validation cannot isolate a naked cell on the native backend at /test"
        );
    }

    #[test]
    fn hermetic_guest_arguments_resolve_repo_inputs_but_keep_dot_local() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-repo-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("tests/e2e")).unwrap();
        fs::write(root.join("README.md"), b"fixture").unwrap();
        let mut argv = vec![
            "tool".into(),
            "tests/e2e".into(),
            "README.md".into(),
            ".".into(),
            "missing/path".into(),
        ];
        resolve_repo_guest_args(&root, &mut argv);
        assert_eq!(argv[0], "tool");
        assert_eq!(argv[1], root.join("tests/e2e").to_string_lossy());
        assert_eq!(argv[2], root.join("README.md").to_string_lossy());
        assert_eq!(argv[3], ".");
        assert_eq!(argv[4], "missing/path");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_dbt_arguments_remain_literal_until_isolation_is_requested() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-dbt-literal-args-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.as_path().join("literal.txt"), b"fixture").unwrap();
        let mut cell = ptrace_cell("verify");
        cell.id.backend = Some("dbt".into());
        cell.test.program = None;
        let original = vec![
            "/bin/echo".to_owned(),
            "literal.txt".to_owned(),
            "./literal.txt".to_owned(),
            ".".to_owned(),
            "missing/path".to_owned(),
        ];
        cell.test.direct = Some(DirectCommand::Argv(original.clone()));
        let mut context = run_context(root.as_path());
        let dir = root.as_path().join("results/cell");
        assert_eq!(prepare_test(&context, &cell, &dir).unwrap(), original);

        context.isolated_workdir = Some(PathBuf::from("/test"));
        let isolated = prepare_test(&context, &cell, &dir).unwrap();
        assert_eq!(
            isolated,
            vec![
                original[0].clone(),
                root.as_path()
                    .join("literal.txt")
                    .to_string_lossy()
                    .into_owned(),
                root.as_path()
                    .join("./literal.txt")
                    .to_string_lossy()
                    .into_owned(),
                original[3].clone(),
                original[4].clone(),
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_fixed_workdir_resolves_repo_inputs_before_chdir() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-ordinary-repo-args-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("fixtures")).unwrap();
        fs::write(root.join("fixtures/input.txt"), b"fixture").unwrap();

        let mut cell = ptrace_cell("verify");
        cell.test.direct = Some(DirectCommand::Argv(vec![
            "/bin/true".into(),
            "fixtures/input.txt".into(),
            ".".into(),
        ]));
        let context = run_context(&root);
        let cell_dir = root.join("results/cell");
        let guest = prepare_test(&context, &cell, &cell_dir).unwrap();
        assert_eq!(guest[1], root.join("fixtures/input.txt").to_string_lossy());
        assert_eq!(guest[2], ".");

        let spec = build_spec(
            &context,
            &cell,
            cell_dir.clone(),
            guest,
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        assert_eq!(
            bound_workdir_source(&spec),
            cell_dir.join("workdir/1"),
            "the ordinary supported path must retain hermit-132's per-attempt bind"
        );
        assert_eq!(
            spec.guest_argv[1],
            root.join("fixtures/input.txt").to_string_lossy(),
            "repo input must be absolute before the guest changes to /tmp/test"
        );
        assert_eq!(spec.guest_argv[2], ".");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verify_relaxations_are_explicit_and_recorded() {
        let mut test = recipe(true);
        let mode = test.modes.get_mut("verify").unwrap();
        mode.compare_io_buffers = Some(false);
        mode.compare_io_buffers_disabled_reason = Some(
            "guest assertions validate sanitizer-specific invariants while whole files may vary"
                .into(),
        );
        mode.rcb_time = Some(false);
        mode.rcb_time_disabled_reason =
            Some("data-dependent assertion work must not perturb virtual time".into());
        validate_mode("fixture/test", "verify", mode, 15).unwrap();
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
        let relaxation = spec
            .argv
            .iter()
            .position(|arg| arg == "--no-detlog-io-buffers")
            .unwrap();
        assert!(relaxation < separator);
        let no_rcb_time = spec
            .argv
            .iter()
            .position(|arg| arg == "--no-rcb-time")
            .unwrap();
        assert!(no_rcb_time < separator);
        assert_eq!(
            cell_relaxations(&cell),
            vec![
                String::from(
                    "--no-detlog-io-buffers: guest assertions validate sanitizer-specific invariants while whole files may vary",
                ),
                String::from(
                    "--no-rcb-time: data-dependent assertion work must not perturb virtual time",
                ),
            ]
        );

        let mut missing_reason = recipe(true).modes.remove("verify").unwrap();
        missing_reason.compare_io_buffers = Some(false);
        assert!(
            validate_mode("fixture/test", "verify", &missing_reason, 15)
                .unwrap_err()
                .contains("requires compare_io_buffers_disabled_reason")
        );

        assert!(
            !spec
                .argv
                .iter()
                .any(|arg| arg.starts_with("--verify-allow"))
        );
        assert_eq!(spec.expected_guest_exit, None);
        let mut expecting = cell.clone();
        expecting
            .test
            .modes
            .get_mut("verify")
            .unwrap()
            .expected_guest_exit = Some(ExpectedGuestExit {
            code: Some(7),
            signal: None,
            reason: "the fixture guest fails on purpose".into(),
        });
        let expecting_spec = build_spec(
            &context,
            &expecting,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            expecting.timeout_seconds,
        )
        .unwrap();
        let separator = expecting_spec
            .argv
            .iter()
            .position(|arg| arg == "--")
            .unwrap();
        let allow = expecting_spec
            .argv
            .iter()
            .position(|arg| arg == "--verify-allow=failure")
            .unwrap();
        assert!(allow < separator);
        assert_eq!(
            expecting_spec
                .expected_guest_exit
                .as_ref()
                .and_then(|e| e.code),
            Some(7)
        );

        let mut missing_rcb_reason = recipe(true).modes.remove("verify").unwrap();
        missing_rcb_reason.rcb_time = Some(false);
        assert!(
            validate_mode("fixture/test", "verify", &missing_rcb_reason, 15)
                .unwrap_err()
                .contains("requires rcb_time_disabled_reason")
        );
    }

    /// A verify cell's guest sees its home, xdg-config and fixture directories
    /// only at the fixed guest paths below `/tmp/e2e`, so the ptrace and
    /// candidate cells of one test, which run from different cell directories,
    /// give their guests the same argv and environment in both runner shapes.
    /// dbt refuses `--bind` and keeps the host paths, as does every mode the
    /// parity post-pass does not compare. The row's own environment keeps the
    /// host paths, which name where the directories are.
    #[test]
    fn verify_cells_give_every_bindable_backend_the_same_guest_inputs() {
        // build_spec creates the sabre path-evidence file, so the root is real.
        let root = std::env::temp_dir().join(format!(
            "hermit-manifest-equalized-inputs-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        for isolated in [None, Some(PathBuf::from("/test"))] {
            let mut context = run_context(&root);
            context.isolated_workdir = isolated.clone();
            let spec_for = |backend: &str, mode: &str| {
                let mut cell = ptrace_cell(mode);
                cell.id.backend = Some(backend.into());
                let dir = cell_artifact_path(&context.result_root, &context.run_id, &cell.id, 1);
                let host = dir.display().to_string();
                let guest = vec![
                    format!("{host}/fixtures/program"),
                    format!("{host}/home/test/.rc"),
                    format!("{host}/xdg-config"),
                    // Embedded and look-alike paths are not rewritten.
                    format!("--input={host}/fixtures/data"),
                    format!("{host}/fixtures-extra"),
                    "plain".into(),
                ];
                let spec = build_spec(&context, &cell, dir, guest, "1", Some(1), 15).unwrap();
                (host, spec)
            };
            let after_separator = |spec: &CellRunSpec| {
                let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
                spec.argv[separator + 1..].to_vec()
            };
            // What the guest can see: argv, environment and each bind target.
            let guest_view = |spec: &CellRunSpec| {
                let separator = spec.argv.iter().position(|arg| arg == "--").unwrap();
                let before = &spec.argv[..separator];
                let targets = before
                    .iter()
                    .filter_map(|arg| arg.strip_prefix("--bind="))
                    .map(|bind| bind.split_once(':').unwrap().1.to_string())
                    .collect::<Vec<_>>();
                (guest_env_args(before), targets, after_separator(spec))
            };

            let (reference_dir, reference) = spec_for("ptrace", "verify");
            let jobs = "1";
            let tmp = if isolated.is_some() {
                "/test"
            } else {
                "/tmp/hermit-e2e"
            };
            assert_equalized_guest_env(&reference.argv, &reference_dir, tmp, jobs);
            assert_eq!(
                reference.guest_argv,
                vec![
                    "/tmp/e2e/fixtures/program".to_string(),
                    "/tmp/e2e/home/test/.rc".to_string(),
                    "/tmp/e2e/xdg-config".to_string(),
                    format!("--input={reference_dir}/fixtures/data"),
                    format!("{reference_dir}/fixtures-extra"),
                    "plain".to_string(),
                ]
            );
            assert_eq!(after_separator(&reference), reference.guest_argv);
            for (name, subdir) in [
                ("HOME", "home"),
                ("XDG_CONFIG_HOME", "xdg-config"),
                ("E2E_FIXTURE_DIR", "fixtures"),
            ] {
                assert_eq!(
                    reference.env.get(name),
                    Some(&format!("{reference_dir}/{subdir}")),
                    "{name}"
                );
            }
            if isolated.is_none() {
                assert_eq!(
                    bound_workdir_source(&reference),
                    PathBuf::from(format!("{reference_dir}/workdir/1"))
                );
            }

            for backend in ["kvm", "liteinst", "sabre"] {
                let (dir, candidate) = spec_for(backend, "verify");
                assert_ne!(dir, reference_dir);
                assert_equalized_guest_env(&candidate.argv, &dir, tmp, jobs);
                // Only the embedded and look-alike arguments still differ.
                let (env, targets, argv) = guest_view(&candidate);
                let (reference_env, reference_targets, reference_argv) = guest_view(&reference);
                assert_eq!((env, targets), (reference_env, reference_targets));
                assert_eq!(argv[..3], reference_argv[..3], "{backend}");
                assert_eq!(argv[5], reference_argv[5], "{backend}");
            }

            let (dir, dbt) = spec_for("dbt", "verify");
            assert_minimal_guest_env(&dbt.argv, &dir, tmp, jobs);
            assert_eq!(dbt.guest_argv[0], format!("{dir}/fixtures/program"));
            assert!(!dbt.argv.iter().any(|arg| arg.starts_with("--bind=")));

            for mode in ["chaos", "replay", "custom"] {
                let (dir, spec) = spec_for("ptrace", mode);
                assert!(
                    !spec
                        .argv
                        .iter()
                        .any(|arg| arg.contains(EQUALIZED_INPUT_ROOT)),
                    "{mode} binds nothing below {EQUALIZED_INPUT_ROOT}: {:?}",
                    spec.argv
                );
                assert_eq!(spec.guest_argv[0], format!("{dir}/fixtures/program"));
            }
        }
        assert!(!equalizes_guest_inputs("verify", "dbt"));
        assert!(!equalizes_guest_inputs("naked", "ptrace"));
        assert!(equalizes_guest_inputs("verify", "ptrace"));
        fs::remove_dir_all(&root).unwrap();
    }

    // The acceptance test of slice S8 of
    // https://github.com/rrnewton/hermit/issues/3301, one test per id
    // measured in
    // https://github.com/rrnewton/hermit/issues/3301#issuecomment-5874842696,
    // each paired with the candidate whose cell directory its second ptrace
    // run is given. In that measurement ptrace given each of these
    // candidates' unequalized inputs diverged from ptrace given its own
    // (control C1: record 14 for five pairs, record 93 for epoll with the
    // sabre directory). Whether an unequalized pair diverges depends on its
    // host path lengths, which here include the test process id: with the
    // equalization disabled, five of these pairs diverged in this layout
    // (records 14, 105 and 107) and epoll with the sabre directory matched by
    // coincidence. So each test also requires the parity record to say the
    // inputs were equalized, which no unequalized pair can.

    #[test]
    fn ptrace_given_kvm_inputs_matches_its_own_on_add_key_enosys() {
        ptrace_given_equalized_inputs_matches_ptrace("c-programs/add-key-enosys", "kvm");
    }

    #[test]
    fn ptrace_given_sabre_inputs_matches_its_own_on_bpf_enosys() {
        ptrace_given_equalized_inputs_matches_ptrace("c-programs/bpf-enosys", "sabre");
    }

    #[test]
    fn ptrace_given_liteinst_inputs_matches_its_own_on_adjtimex_deterministic() {
        ptrace_given_equalized_inputs_matches_ptrace(
            "c-programs/adjtimex-deterministic",
            "liteinst",
        );
    }

    #[test]
    fn ptrace_given_kvm_inputs_matches_its_own_on_ioctl_fioclex() {
        ptrace_given_equalized_inputs_matches_ptrace("c-programs/ioctl-fioclex", "kvm");
    }

    #[test]
    fn ptrace_given_liteinst_inputs_matches_its_own_on_mmap_determinism() {
        ptrace_given_equalized_inputs_matches_ptrace("c-programs/mmap-determinism", "liteinst");
    }

    #[test]
    fn ptrace_given_sabre_inputs_matches_its_own_on_epoll_determinism() {
        ptrace_given_equalized_inputs_matches_ptrace("c-programs/epoll-determinism", "sabre");
    }

    /// The Hermit the acceptance test runs: `HERMIT_BIN`, else the verified
    /// artifact that validate publishes before `test.regular_crates`
    /// (`target/ci/hermit-e2e-artifact.path`), else `target/debug/hermit`.
    /// Without one the test fails, because a pass that ran no guest would
    /// measure nothing.
    fn acceptance_hermit(root: &Path) -> PathBuf {
        if let Some(bin) = std::env::var_os("HERMIT_BIN") {
            return PathBuf::from(bin);
        }
        let pointer = root.join("target/ci/hermit-e2e-artifact.path");
        if pointer.is_file() {
            let output = Command::new(root.join("ci/verify-hermit-e2e-artifact.sh"))
                .arg(&pointer)
                .output()
                .expect("run ci/verify-hermit-e2e-artifact.sh");
            assert!(
                output.status.success(),
                "{} names no verified Hermit artifact:\n{}",
                pointer.display(),
                String::from_utf8_lossy(&output.stderr)
            );
            return PathBuf::from(String::from_utf8(output.stdout).unwrap().trim()).join("hermit");
        }
        let debug = root.join("target/debug/hermit");
        assert!(
            debug.is_file(),
            "the equal-inputs acceptance test runs a real Hermit: set HERMIT_BIN, publish \
             {} (validate's build.e2e_artifact does), or build {}",
            pointer.display(),
            debug.display()
        );
        debug
    }

    /// Ptrace given `candidate`'s cell inputs through the equalization matches
    /// ptrace given its own on `test`, so a parity comparison of the two earns
    /// full credit with equal inputs. It runs the id's real ptrace verify cell
    /// twice, once in its own cell directory and once in the candidate's, and
    /// hands both rows to the parity post-pass with the second relabelled as
    /// the candidate. The runner shape follows the environment, so validate's
    /// `HERMIT_E2E_EMPTY_WORKDIR=/test` runs the hermetic shape. One id per
    /// test keeps each inside the per-test Nextest CPU bound (22 CPU-s): with
    /// the debug Hermit that validate publishes, hashing that binary three
    /// times per cell costs more CPU than the guest runs do.
    fn ptrace_given_equalized_inputs_matches_ptrace(test: &str, candidate: &str) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = root.canonicalize().unwrap();
        let hermit_bin = acceptance_hermit(&root);
        let scratch = root.join(format!(
            "target/equalized-input-acceptance-{}-{}",
            std::process::id(),
            test.replace('/', "-")
        ));
        let _ = fs::remove_dir_all(&scratch);
        let isolated_workdir = std::env::var_os(ISOLATED_WORKDIR_ENV).map(|value| {
            assert_eq!(value, HERMETIC_TEST_WORKDIR, "{ISOLATED_WORKDIR_ENV}");
            PathBuf::from(HERMETIC_TEST_WORKDIR)
        });
        let context = RunContext {
            root: root.clone(),
            hermit_bin: hermit_bin.clone(),
            result_root: scratch.join("results"),
            build_root: scratch.join("build"),
            run_id: "equalized-inputs".into(),
            machine_shortname: "acceptance".into(),
            kernel_version: command_text("uname", &["-r"]).unwrap(),
            host_capabilities: probe_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: resolve_run_epoch(std::env::var_os("HERMIT_EPOCH"), SystemTime::now).unwrap(),
            source_sha: "0".repeat(40),
            source_dirty: false,
            binary_build_sha: probe_binary_build_sha(&hermit_bin),
            prebuilt: false,
            keep_logs: true,
            parity_retained: BTreeSet::new(),
            run_verify_strict: command_help_contains(
                &hermit_bin,
                &["run", "--help"],
                "--verify-strict",
            )
            .unwrap(),
            record_verify_strict: false,
            timeout_multipliers: crate::timeouts::timeout_multipliers_from_env().unwrap(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir,
        };
        let manifests = ManifestSet::load(&root).unwrap();
        let [cell] = manifests
            .select(&Selection {
                population: Some(Population::Enabled),
                test: Some(test.into()),
                mode: Some("verify".into()),
                backend: Some("ptrace".into()),
                ..Selection::default()
            })
            .unwrap()
            .try_into()
            .unwrap_or_else(|cells: Vec<_>| panic!("{test}: {} ptrace verify cells", cells.len()));
        let own = run_cell(&context, &cell)
            .unwrap_or_else(|failure| panic!("{test} in its own directory: {}", failure.reason));
        let candidate_id = CellId {
            backend: Some(candidate.into()),
            ..cell.id.clone()
        };
        let candidate_dir = cell_artifact_path(
            &context.result_root,
            &context.run_id,
            &candidate_id,
            context.attempt,
        );
        let mut given =
            run_cell_in(&context, &cell, candidate_dir.clone()).unwrap_or_else(|failure| {
                panic!("{test} in the {candidate} directory: {}", failure.reason)
            });
        for (row, dir) in [
            (&own, cell_artifact_dir(&context, &cell)),
            (&given, candidate_dir),
        ] {
            assert_eq!(row.outcome, "PASS", "{test}: {row:?}");
            // The inputs came from that cell's directory.
            assert_eq!(row.artifact_dir, dir.to_string_lossy(), "{test}");
            assert_eq!(
                row.env.get("E2E_FIXTURE_DIR"),
                Some(&format!("{}/fixtures", dir.display())),
                "{test}"
            );
        }
        given.backend = Some(candidate.into());

        let scope = BTreeSet::from([crate::parity::ParityCellId {
            test_id: test.into(),
            backend: crate::parity::ParityBackend::parse(candidate).unwrap(),
        }]);
        let artifacts = scratch.join("parity");
        fs::create_dir_all(&artifacts).unwrap();
        let report = crate::parity::post_pass(
            &crate::parity::PostPassConfig::new(
                &artifacts,
                &hermit_bin,
                &context.run_id,
                &context.source_sha,
            ),
            &scope,
            &[own, given],
        )
        .unwrap();
        assert_eq!(report.log_diff_runs, 1, "{test}");
        let [record] = report.records.as_slice() else {
            panic!("{test}: {} parity records", report.records.len());
        };
        record.validate().unwrap();
        assert_eq!(
            record.verdict,
            crate::parity::ParityVerdict::Matched,
            "{record:?}"
        );
        assert!(record.inputs_equalized, "{record:?}");
        assert_eq!(record.credit, Some(1.0), "{record:?}");
        assert_eq!(record.unequalized_credit, None, "{record:?}");
        // A match of two near-empty logs would prove nothing. The measurement
        // above recorded 115 to 173 records for these ids.
        assert!(
            record.left_len.is_some_and(|len| len >= 100)
                && record.left_len == record.right_len
                && record.matched_prefix == record.left_len,
            "{record:?}"
        );
        assert!(
            report.summary_line().contains(
                "mean credit 1.0000 over 1 measured with equal inputs; \
                 none measured with unequal inputs"
            ),
            "{}",
            report.summary_line()
        );
        fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn run_spec_records_correct_policy_without_hidden_portable_flags() {
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: PathBuf::from("/repo"),
            hermit_bin: PathBuf::from("/repo/hermit"),
            result_root: PathBuf::from("/repo/results"),
            build_root: PathBuf::from("/repo/build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: true,
            record_verify_strict: true,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(7),
            isolated_workdir: None,
        };
        let spec = build_spec(
            &context,
            &cell,
            PathBuf::from("/repo/results/cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        assert!(spec.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(spec.argv.iter().any(|arg| arg == "--base-env=minimal"));
        assert_eq!(
            spec.env.get(SCHEDULED_JOBS_ENV).map(String::as_str),
            Some("7")
        );
        assert_equalized_guest_env(&spec.argv, "/repo/results/cell", "/tmp/hermit-e2e", "7");
        assert!(!spec.argv.iter().any(|arg| arg == "--no-virtualize-cpuid"));
        assert!(
            !spec
                .argv
                .iter()
                .any(|arg| arg == "--max-timeslice=disabled")
        );
        for name in ["RUSTUP_HOME", "CARGO_HOME"] {
            assert!(!spec.env.contains_key(name));
            assert!(
                !spec
                    .argv
                    .iter()
                    .any(|arg| arg.starts_with(&format!("{name}=")))
            );
        }

        let mut replay_test = recipe(true);
        let replay_mode = replay_test.modes.remove("verify").unwrap();
        replay_test.modes.insert("replay".into(), replay_mode);
        let replay_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: replay_test.id.clone(),
                mode: "replay".into(),
                backend: Some("ptrace".into()),
            },
            test: replay_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let replay = build_spec(
            &context,
            &replay_cell,
            PathBuf::from("/repo/results/replay-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            replay_cell.timeout_seconds,
        )
        .unwrap();
        assert!(replay.argv.iter().any(|arg| arg == "--verify-strict"));
        assert!(replay.argv.windows(2).any(|window| {
            window[0] == "--verify-json" && window[1] == "/repo/results/replay-cell/verify-1.json"
        }));
        assert!(!replay.argv.iter().any(|arg| arg.starts_with("--base-env")));
        assert_eq!(
            guest_env_args(&replay.argv),
            vec![
                "LC_ALL=C".to_string(),
                "TZ=UTC".to_string(),
                "HOME=/repo/results/replay-cell/home".to_string(),
                "XDG_CONFIG_HOME=/repo/results/replay-cell/xdg-config".to_string(),
                "E2E_TMPDIR=/tmp/hermit-e2e".to_string(),
                "E2E_FIXTURE_DIR=/repo/results/replay-cell/fixtures".to_string(),
                "HERMIT_E2E_SCHEDULED_JOBS=7".to_string(),
            ]
        );

        let mut custom_test = recipe(true);
        let mut custom_mode = custom_test.modes.remove("verify").unwrap();
        custom_mode.args = vec!["--base-env=minimal".into()];
        custom_test.modes.insert("custom".into(), custom_mode);
        let custom_cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: custom_test.id.clone(),
                mode: "custom".into(),
                backend: Some("ptrace".into()),
            },
            test: custom_test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let custom = build_spec(
            &context,
            &custom_cell,
            PathBuf::from("/repo/results/custom-cell"),
            vec!["/bin/true".into()],
            "1",
            None,
            custom_cell.timeout_seconds,
        )
        .unwrap();
        assert_eq!(
            guest_env_args(&custom.argv),
            vec!["HERMIT_E2E_SCHEDULED_JOBS=7".to_string()]
        );
    }

    #[test]
    fn preparation_preserves_toolchain_homes_outside_the_guest_environment() {
        let mut derived = cell_env(Path::new("/cell"), false);
        add_toolchain_homes(
            &mut derived,
            None,
            None,
            Some(OsStr::new("/example/toolchain-root")),
        );
        assert_eq!(derived["RUSTUP_HOME"], "/example/toolchain-root/.rustup");
        assert_eq!(derived["CARGO_HOME"], "/example/toolchain-root/.cargo");

        let mut explicit = cell_env(Path::new("/cell"), false);
        add_toolchain_homes(
            &mut explicit,
            Some(OsStr::new("/toolchains/rustup")),
            Some(OsStr::new("/toolchains/cargo")),
            Some(OsStr::new("/example/toolchain-root")),
        );
        assert_eq!(explicit["RUSTUP_HOME"], "/toolchains/rustup");
        assert_eq!(explicit["CARGO_HOME"], "/toolchains/cargo");

        let guest = cell_env(Path::new("/cell"), false);
        assert!(!guest.contains_key("RUSTUP_HOME"));
        assert!(!guest.contains_key("CARGO_HOME"));
    }

    #[test]
    fn recorded_shell_command_replays_literal_environment_and_argv() {
        let root = std::env::temp_dir().join(format!(
            "hermit-recorded-command-bracket-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let env = BTreeMap::from([("E2E_VALUE".into(), "space and ' quote".into())]);
        let argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "test \"$E2E_VALUE\" = \"space and ' quote\" && test \"$1\" = \"arg with spaces\""
                .into(),
            "recorded-command".into(),
            "arg with spaces".into(),
        ];
        let literal = shell_command(&root.to_string_lossy(), &env, &argv);
        let status = Command::new("/bin/sh")
            .args(["-c", &literal])
            .status()
            .unwrap();
        assert!(status.success(), "recorded command failed: {literal}");
        fs::remove_dir_all(root).unwrap();
    }

    fn attempt_with_divergence(
        turn: Option<u64>,
        nanos: Option<u64>,
        record: Option<u64>,
        syscall: Option<u64>,
    ) -> AttemptResult {
        let mut attempt = attempt_with_sabre_evidence("");
        attempt.first_divergent_scheduler_turn = turn;
        attempt.first_divergent_virtual_nanoseconds = nanos;
        attempt.first_divergent_record = record;
        attempt.first_divergent_syscall = syscall;
        attempt
    }

    /// The cell-level position is the FIRST attempt that located one, matching
    /// how `reason` is chosen, and NOT the last -- otherwise a passing retry
    /// would erase the position the failing attempt found.
    #[test]
    fn cell_divergence_takes_the_first_attempt_that_located_one() {
        let mut attempts = vec![
            attempt_with_divergence(None, None, None, None),
            attempt_with_divergence(Some(4), Some(400), Some(40), Some(14)),
            attempt_with_divergence(Some(1), Some(100), Some(10), Some(2)),
        ];
        attempts[1].first_divergent_left_message = Some("left event A".into());
        attempts[1].first_divergent_right_message = Some("right event A".into());
        attempts[2].first_divergent_left_message = Some("left event B".into());
        attempts[2].first_divergent_right_message = Some("right event B".into());
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition {
                scheduler_turn: Some(4),
                virtual_nanoseconds: Some(400),
                record: Some(40),
                syscall: Some(14),
                left_message: Some("left event A".into()),
                right_message: Some("right event A".into()),
            },
            "the first located position wins; a later, earlier-diverging attempt \
             must not silently replace it, and this is NOT a min across attempts"
        );
    }

    /// Resolved per coordinate, so a report that located only some of the four
    /// still contributes the ones it has. Each coordinate here comes from a
    /// DIFFERENT attempt, which a per-attempt rule could not produce.
    #[test]
    fn cell_divergence_resolves_each_coordinate_independently() {
        let attempts = vec![
            attempt_with_divergence(None, Some(900), None, None),
            attempt_with_divergence(Some(7), None, None, None),
            attempt_with_divergence(None, None, Some(3), None),
            attempt_with_divergence(None, None, None, Some(5)),
        ];
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition {
                scheduler_turn: Some(7),
                virtual_nanoseconds: Some(900),
                record: Some(3),
                syscall: Some(5),
                left_message: None,
                right_message: None,
            }
        );
    }

    /// A located record with NO syscall is a real state, not a gap: a run can
    /// diverge before any syscall has completed. Measured on a real log --
    /// diverging at record 12 reported syscall `None`, because no
    /// `finish syscall #N` had been written yet.
    #[test]
    fn a_divergence_before_any_syscall_completes_reports_no_syscall() {
        let attempts = vec![attempt_with_divergence(Some(1), None, Some(12), None)];
        let position = cell_divergence_position(&attempts);
        assert_eq!(position.record, Some(12));
        assert_eq!(position.syscall, None);
    }

    /// A clean cell reports no position at all.
    #[test]
    fn cell_divergence_is_absent_when_no_attempt_located_one() {
        let attempts = vec![attempt_with_divergence(None, None, None, None)];
        assert_eq!(
            cell_divergence_position(&attempts),
            DivergencePosition::default()
        );
        assert_eq!(cell_divergence_position(&[]), DivergencePosition::default());
    }

    /// ⚠️ ABSENCE OF THIS KEY IS RESERVED FOR "WRITTEN BEFORE THE FIELD
    /// EXISTED", AND NOTHING ELSE. A row that ran, compared, and located no
    /// divergence must carry the key with an explicit null.
    ///
    /// WHY IT NEEDS A TEST RATHER THAN BEING OBVIOUS. `CELL_RESULT_SCHEMA` was
    /// NOT bumped when these coordinates were added, so the schema number
    /// cannot separate the two meanings. Within schema 4 the only thing that
    /// distinguishes "this predates the field" from "this ran and found nothing"
    /// is whether the key is present at all.
    ///
    /// ⚠️ THE STRUCTURAL CLAIM IS THE DURABLE ONE; THE COUNTS ARE NOT. Schema 4
    /// contains BOTH rows with the key and rows without it, while schemas 1, 2
    /// and 3 never carry it -- that is what makes the schema number useless as a
    /// discriminator, and agent(hermit-106) reproduced it independently across
    /// 1478 rows. The row COUNTS behind it move: this file measured 28899 without
    /// and 17540 with over 6215 retained `results.jsonl`, and a sample of 1200 of
    /// those files gave a different ratio. Both are honest. The corpus lives in
    /// gitignored run directories that agents create and delete, so it is not a
    /// fixed population and a count taken from it is true of one moment. Do not
    /// treat these numbers as a baseline to compare against.
    ///
    /// That distinction currently holds only because no
    /// `skip_serializing_if = "Option::is_none"` is attached to these
    /// fields -- an entirely natural tidy-up for a field that is null on the
    /// large majority of rows. Adding one would silently convert every future
    /// "ran and found nothing" row into a row indistinguishable from a
    /// pre-field one, and no existing test would notice. This is that test.
    ///
    /// It asserts PRESENCE and NULLNESS separately from any value, so it fails
    /// on the skip attribute rather than on a changed position.
    /// ⚠️ THE ROW, NOT ONLY THE ATTEMPT. The measurement in the docstring above is
    /// about `results.jsonl` ROWS, and a row is a `CellResult` with its attempts
    /// nested inside it. An earlier version of this test guarded `AttemptResult`
    /// alone, which left the level the evidence actually describes uncovered:
    /// agent(hermit-106) put `skip_serializing_if = "Option::is_none"` on all four
    /// `CellResult` coordinates and the whole package stayed GREEN, 126 tests, 0
    /// failures, INCLUDING this test. A check that stays green while the hazard it
    /// names is open is the failure this file is full of warnings about.
    fn cell_result_that_located_nothing() -> CellResult {
        CellResult {
            cpu_observations: None,
            schema: CELL_RESULT_SCHEMA,
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            hermit_sha: "sha".into(),
            binary_build_sha: None,
            source_tree_dirty: false,
            binary_sha256: None,
            test_sha256: "digest".into(),
            test: "fixture/test".into(),
            category: "fixture".into(),
            lane: "portable".into(),
            mode: "verify".into(),
            backend: Some("ptrace".into()),
            classification: "required".into(),
            outcome: "PASS".into(),
            result: Some(ObservedResult::Pass),
            failure_class: None,
            error_kind: None,
            timeout_seconds: 3,
            execution_cpu_timeout_seconds: Some(1),
            execution_wall_timeout_seconds: Some(3),
            duration_ms: Some(1),
            cpu_usage_usec: Some(1),
            runtime: None,
            log_level: None,
            effective_args: Vec::new(),
            argv: vec!["hermit".into()],
            guest_argv: vec!["guest".into()],
            env: BTreeMap::new(),
            cwd: "/repo".into(),
            shell_command: "cd /repo && hermit".into(),
            relaxations: Vec::new(),
            execution_path: None,
            diversity: None,
            attempts: vec![attempt_with_sabre_evidence("evidence")],
            backend_parity: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            reason: None,
            artifact_dir: "/repo/artifacts".into(),
            expected_guest_exit: None,
        }
    }

    #[test]
    fn only_a_reasoned_product_failure_of_a_diagnostic_cell_is_a_diagnostic_failure() {
        let reason = "bounded probe";
        let diagnostic = |outcome: &str, class: Option<FailureClass>, relaxations: Vec<String>| {
            let mut row = cell_result_that_located_nothing();
            row.classification = "diagnostic".into();
            row.outcome = outcome.into();
            row.failure_class = class;
            row.relaxations = relaxations;
            row
        };
        let recorded = || vec![format!("{DIAGNOSTIC_RELAXATION_PREFIX}{reason}")];
        let failure = diagnostic("FAIL", Some(FailureClass::ProductFailure), recorded());
        assert_eq!(
            diagnostic_failure_reason(std::slice::from_ref(&failure)),
            Some(reason)
        );
        // The cell exceeding its own budget is a measured failure too.
        let mut timeout = diagnostic("FAIL", Some(FailureClass::NoResult), recorded());
        timeout.result = Some(ObservedResult::Timeout);
        timeout.error_kind = Some("wall-timeout".into());
        assert_eq!(diagnostic_failure_reason(&[timeout]), Some(reason));
        assert_eq!(diagnostic_failure_reason(&[]), None);
        // A retried product failure that ends in an ERROR has no terminal product verdict.
        let mut error_retry = diagnostic("ERROR", Some(FailureClass::NoResult), recorded());
        error_retry.attempt = 2;
        assert_eq!(
            diagnostic_failure_reason(&[failure.clone(), error_retry]),
            None
        );
        let mut fail_retry = failure.clone();
        fail_retry.attempt = 2;
        assert_eq!(
            diagnostic_failure_reason(&[failure.clone(), fail_retry]),
            Some(reason)
        );
        let mut required = failure.clone();
        required.classification = "required".into();
        for (name, row) in [
            ("a required cell", required),
            ("a pass", diagnostic("PASS", None, recorded())),
            (
                "an error",
                diagnostic("ERROR", Some(FailureClass::NoResult), recorded()),
            ),
            (
                "a FAIL with no product verdict that is not a timeout",
                diagnostic("FAIL", Some(FailureClass::NoResult), recorded()),
            ),
            (
                "an infrastructure failure",
                diagnostic(
                    "FAIL",
                    Some(FailureClass::UnderstoodInfrastructureFailure),
                    recorded(),
                ),
            ),
            (
                "a row that lost its reason",
                diagnostic("FAIL", Some(FailureClass::ProductFailure), Vec::new()),
            ),
            (
                "a row with two reasons",
                diagnostic(
                    "FAIL",
                    Some(FailureClass::ProductFailure),
                    [recorded(), recorded()].concat(),
                ),
            ),
        ] {
            assert_eq!(diagnostic_failure_reason(&[row]), None, "{name}");
        }
    }

    #[test]
    fn a_located_nothing_row_still_carries_all_divergence_fields() {
        let attempt = attempt_with_sabre_evidence("evidence");
        let rendered = serde_json::to_value(&attempt).expect("attempt serializes");
        let object = rendered.as_object().expect("attempt is a JSON object");
        for key in [
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            let found = object.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} is absent from a serialized attempt that located nothing. \
                     Absence is how a consumer recognises a row written before this \
                     field existed; emitting it for a row that simply found nothing \
                     merges two different facts that no later reader can separate."
                )
            });
            assert!(
                found.is_null(),
                "{key} must be an explicit null when nothing was located, got {found}"
            );
        }

        // THE ROW LEVEL, which is what a `results.jsonl` line actually is and what
        // every count in the docstring above was measured over.
        let row = cell_result_that_located_nothing();
        let rendered = serde_json::to_value(&row).expect("row serializes");
        let object = rendered.as_object().expect("row is a JSON object");
        assert!(
            !object.contains_key("run_index"),
            "ordinary validate rows must not invent a pressure repetition"
        );
        assert_eq!(object["machine_shortname"], "fixture-host");
        assert_eq!(object["kernel_version"], "7.1.3-fixture");
        assert_eq!(
            object["host_capabilities"]["cpuid-faulting"]["present"],
            true
        );
        assert_eq!(object["host_capabilities"]["kvm"]["present"], false);
        for key in [
            "first_divergent_record",
            "first_divergent_syscall",
            "first_divergent_scheduler_turn",
            "first_divergent_virtual_nanoseconds",
            "first_divergent_left_message",
            "first_divergent_right_message",
        ] {
            let found = object.get(key).unwrap_or_else(|| {
                panic!(
                    "{key} is absent from a serialized RESULT ROW that located nothing. \
                     A results.jsonl line is a CellResult, so this is the level every \
                     count in this test's docstring was measured over; absence here is \
                     how a reader recognises a row written before the field existed."
                )
            });
            assert!(
                found.is_null(),
                "{key} must be an explicit null on the row when nothing was located, got {found}"
            );
        }

        let decoded: CellResult =
            serde_json::from_value(rendered).expect("the producer-owned result type reads its row");
        assert_eq!(decoded.attempt, 1);
        assert_eq!(decoded.run_index, None);
        assert_eq!(decoded.attempts.len(), 1);

        let mut pressure_row = row;
        pressure_row.run_index = Some(4);
        let rendered = serde_json::to_value(&pressure_row).expect("pressure row serializes");
        assert_eq!(rendered["run_index"], 4);
        let decoded: CellResult = serde_json::from_value(rendered)
            .expect("the producer-owned result type reads a pressure row");
        assert_eq!(decoded.run_index, Some(4));
    }

    #[test]
    fn schema4_keeps_its_wall_field_and_adds_explicit_execution_bounds() {
        let row = cell_result_that_located_nothing();
        let mut rendered = serde_json::to_value(&row).expect("cell result serializes");
        assert_eq!(rendered["timeout_seconds"], 3);
        assert_eq!(rendered["execution_cpu_timeout_seconds"], 1);
        assert_eq!(rendered["execution_wall_timeout_seconds"], 3);
        row.require_current_timeout_policy().unwrap();

        let mut half_present = row.clone();
        half_present.execution_wall_timeout_seconds = None;
        assert!(half_present.validate_timeout_policy().is_err());
        let publication = std::env::temp_dir().join(format!(
            "hermit-runner-timeout-policy-publication-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&publication);
        assert!(append_result(&publication, &half_present).is_err());
        assert!(
            !publication.exists(),
            "a malformed current timeout policy reached the results file"
        );
        let mut wrong_cpu = row.clone();
        wrong_cpu.execution_cpu_timeout_seconds = Some(3);
        assert!(wrong_cpu.validate_timeout_policy().is_err());
        let mut wrong_wall = row.clone();
        wrong_wall.execution_wall_timeout_seconds = Some(2);
        assert!(wrong_wall.validate_timeout_policy().is_err());

        let object = rendered.as_object_mut().unwrap();
        object.remove("execution_cpu_timeout_seconds");
        object.remove("execution_wall_timeout_seconds");
        let retained: CellResult = serde_json::from_value(rendered)
            .expect("a schema-4 row written before the additive fields remains readable");
        assert_eq!(retained.timeout_seconds, 3);
        assert_eq!(retained.execution_cpu_timeout_seconds, None);
        assert_eq!(retained.execution_wall_timeout_seconds, None);
        retained.validate_timeout_policy().unwrap();
        assert!(retained.require_current_timeout_policy().is_err());
    }

    fn attempt_with_sabre_evidence(evidence: &str) -> AttemptResult {
        AttemptResult {
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            index: "1".into(),
            outcome: "PASS".into(),
            error_kind: None,
            status: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 1,
            cpu_usage_usec: Some(1),
            observation_sha256: Some("a".into()),
            argv: vec![
                "hermit".into(),
                "--backend".into(),
                "sabre".into(),
                "--verify".into(),
            ],
            guest_argv: vec!["guest".into()],
            env: BTreeMap::new(),
            cwd: "/repo".into(),
            shell_command: "cd /repo && env hermit --backend sabre --verify".into(),
            stdout: String::new(),
            stderr: String::new(),
            verification_report: None,
            verification_report_sha256: None,
            runtime: None,
            sabre_path_evidence: Some(evidence.into()),
            sabre_path_evidence_sha256: Some("b".into()),
            reason: None,
        }
    }

    #[test]
    fn current_cell_result_carries_result_and_failure_class() {
        let passed = serde_json::to_value(cell_result_that_located_nothing()).unwrap();
        assert_eq!(passed["result"], "pass");
        assert_eq!(passed["failure_class"], JsonValue::Null);

        let mut failed = cell_result_that_located_nothing();
        failed.outcome = "FAIL".into();
        failed.result = Some(ObservedResult::DeterminismFailure);
        failed.failure_class = Some(FailureClass::ProductFailure);
        let failed = serde_json::to_value(failed).unwrap();
        assert_eq!(failed["result"], "determinism-failure");
        assert_eq!(failed["failure_class"], "product_failure");

        let mut missing = cell_result_that_located_nothing();
        missing.outcome = "FAIL".into();
        missing.result = Some(ObservedResult::DeterminismFailure);
        missing.failure_class = None;
        assert_eq!(
            missing.require_current_classification().unwrap_err(),
            "FAIL result has no failure_class; current non-passes must be attributed"
        );

        let mut legacy = cell_result_that_located_nothing();
        legacy.outcome = "FAIL".into();
        legacy.result = None;
        legacy.failure_class = None;
        legacy
            .validate_recorded_classification()
            .expect("retained pre-field schema-4 row remains readable");
        assert!(legacy.require_current_classification().is_err());

        let mut error_with_product_result = cell_result_that_located_nothing();
        error_with_product_result.outcome = "ERROR".into();
        error_with_product_result.result = Some(ObservedResult::CrashError);
        error_with_product_result.failure_class = Some(FailureClass::ProductFailure);
        assert_eq!(
            error_with_product_result
                .require_current_classification()
                .unwrap_err(),
            "ERROR result must carry a non-product observation/classification, got (Some(CrashError), Some(ProductFailure))"
        );

        let mut timeout = cell_result_that_located_nothing();
        timeout.outcome = "ERROR".into();
        timeout.result = Some(ObservedResult::Timeout);
        timeout.failure_class = Some(FailureClass::NoResult);
        timeout.require_current_classification().unwrap();

        let mut sandbox_denied = cell_result_that_located_nothing();
        sandbox_denied.outcome = "ERROR".into();
        sandbox_denied.result = Some(ObservedResult::SandboxDenied);
        sandbox_denied.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
        sandbox_denied
            .require_current_classification()
            .expect("typed sandbox denial is a current non-product result");
    }

    #[test]
    fn framework_classifies_divergence_and_crash_before_pressure_reads_them() {
        let mut divergence = attempt_with_sabre_evidence("");
        divergence.outcome = "FAIL".into();
        let mut report = canonical_verification_report();
        report.verified = false;
        report.bitwise_parity = false;
        report.verdict = Verdict::Diverged;
        report.first_divergent_scheduler_turn = Some(4);
        report.first_divergent_virtual_nanoseconds = Some(7);
        report.first_divergent_record = Some(9);
        report.first_divergent_syscall = Some(2);
        report.first_divergent_left_message = Some("left".into());
        report.first_divergent_right_message = Some("right".into());
        divergence.verification_report = Some(serde_json::to_string(&report).unwrap());
        let divergence_result = observed_result(
            "verify",
            &divergence.outcome,
            std::slice::from_ref(&divergence),
            divergence.error_kind.as_deref(),
        );
        assert_eq!(divergence_result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(
            failure_class(
                &divergence.outcome,
                divergence_result,
                divergence.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );

        let mut crash = attempt_with_sabre_evidence("");
        crash.outcome = "FAIL".into();
        crash.status = Some(1);
        let crash_result = observed_result(
            "verify",
            &crash.outcome,
            std::slice::from_ref(&crash),
            crash.error_kind.as_deref(),
        );
        assert_eq!(crash_result, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(&crash.outcome, crash_result, crash.error_kind.as_deref()),
            Some(FailureClass::ProductFailure)
        );

        let mut invalidated = divergence.clone();
        invalidated.outcome = "ERROR".into();
        invalidated.error_kind = Some("infrastructure".into());
        let invalidated_result = observed_result(
            "verify",
            &invalidated.outcome,
            std::slice::from_ref(&invalidated),
            invalidated.error_kind.as_deref(),
        );
        assert_eq!(
            invalidated_result, None,
            "a later infrastructure failure must outrank an earlier product observation"
        );
        assert_eq!(
            failure_class(
                &invalidated.outcome,
                invalidated_result,
                invalidated.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut invalid_evidence = divergence;
        invalid_evidence.outcome = "ERROR".into();
        invalid_evidence.error_kind = Some("invalid-backend-evidence".into());
        let invalid_evidence_result = observed_result(
            "verify",
            &invalid_evidence.outcome,
            std::slice::from_ref(&invalid_evidence),
            invalid_evidence.error_kind.as_deref(),
        );
        assert_eq!(invalid_evidence_result, None);
        assert_eq!(
            failure_class(
                &invalid_evidence.outcome,
                invalid_evidence_result,
                invalid_evidence.error_kind.as_deref()
            ),
            Some(FailureClass::NoResult)
        );
    }

    #[test]
    fn framework_keeps_typed_product_results_with_incidental_environment_text() {
        let report = r#"{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1","display_name":"BitwiseInfoV1","compare_io_buffers":true,"log_scope":"info","virtualize_time":true,"strip_lines":false,"canonicalize_addresses":true,"full_trace":true,"exact_remainder":true,"stripped_prefixes":["real-wall-clock-prefix/v1"],"canonicalizations":["host-address-to-first-appearance-ordinal/v1"],"ignore_lines":false,"skip_commit":false,"skip_detlog":false},"compared_log_messages":{"left":1,"right":1},"first_divergent_scheduler_turn":4,"first_divergent_virtual_nanoseconds":7,"first_divergent_record":9,"first_divergent_syscall":2,"first_divergent_left_message":"left","first_divergent_right_message":"right","compared_outputs":{"left":{"exit_code":null,"signal":null,"stdout_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdout_bytes":0,"stderr_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","stderr_bytes":0},"right":{"exit_code":null,"signal":null,"stdout_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdout_bytes":0,"stderr_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","stderr_bytes":0}},"no_result_reason":null,"infrastructure_error":null,"guest_exit_code":null,"guest_signal":null}"#;
        current_verification_report(report.as_bytes())
            .expect("the product-precedence fixture must be a current report")
            .require_canonical_comparison()
            .expect("the product-precedence fixture must retain canonical evidence");
        for banner in [
            "An action was blocked on this server based on a security policy!",
            "fatal: Could not resolve proxy",
        ] {
            for (mode, expected) in [
                ("verify", ObservedResult::DeterminismFailure),
                ("replay", ObservedResult::ReplayFailure),
            ] {
                for earlier in [false, true] {
                    let mut divergence = attempt_with_sabre_evidence("");
                    divergence.outcome = "FAIL".into();
                    divergence.status = Some(1);
                    divergence.verification_report = Some(report.into());
                    let mut diagnostic = divergence.clone();
                    diagnostic.verification_report = None;
                    diagnostic.stderr = banner.into();
                    let attempts = if earlier {
                        vec![diagnostic, divergence]
                    } else {
                        divergence.stderr = banner.into();
                        vec![divergence]
                    };
                    let bytes = serde_json::to_vec(&attempts).unwrap();
                    let result = observed_result(mode, "FAIL", &attempts, None);
                    assert_eq!(
                        result,
                        Some(expected),
                        "{mode}, earlier={earlier}, {banner}"
                    );
                    assert_eq!(
                        failure_class("FAIL", result, None),
                        Some(FailureClass::ProductFailure)
                    );
                    assert_eq!(serde_json::to_vec(&attempts).unwrap(), bytes);
                }
                let mut passed = attempt_with_sabre_evidence("");
                passed.stderr = banner.into();
                assert_eq!(
                    observed_result(mode, "PASS", &[passed], None),
                    Some(ObservedResult::Pass)
                );
            }
        }
    }

    #[test]
    fn framework_terminal_refusal_cannot_revive_incidental_divergence() {
        let mut divergence = attempt_with_sabre_evidence("");
        divergence.outcome = "FAIL".into();
        divergence.status = Some(1);
        divergence.verification_report = Some(
            r#"{"verified":false,"bitwise_parity":false,"verdict":"diverged","comparison":{"strictness":"canonical","compare_logs":true,"record_envelope":"all_records_v1","display_name":"BitwiseInfoV1","compare_io_buffers":true,"log_scope":"info","virtualize_time":true,"strip_lines":false,"canonicalize_addresses":true,"full_trace":true,"exact_remainder":true,"stripped_prefixes":["real-wall-clock-prefix/v1"],"canonicalizations":["host-address-to-first-appearance-ordinal/v1"],"ignore_lines":false,"skip_commit":false,"skip_detlog":false},"compared_log_messages":{"left":1,"right":1},"first_divergent_scheduler_turn":4,"first_divergent_virtual_nanoseconds":7,"first_divergent_record":9,"first_divergent_syscall":2,"first_divergent_left_message":"left","first_divergent_right_message":"right","compared_outputs":{"left":{"exit_code":null,"signal":null,"stdout_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdout_bytes":0,"stderr_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","stderr_bytes":0},"right":{"exit_code":null,"signal":null,"stdout_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stdout_bytes":0,"stderr_sha256":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","stderr_bytes":0}},"no_result_reason":null,"infrastructure_error":null,"guest_exit_code":null,"guest_signal":null}"#.into(),
        );
        current_verification_report(divergence.verification_report.as_ref().unwrap().as_bytes())
            .expect("the terminal-refusal fixture must start with a valid current divergence")
            .require_canonical_comparison()
            .expect("the terminal-refusal fixture must retain canonical evidence");
        for error in [
            "infrastructure",
            "result-publication",
            "invalid-backend-evidence",
            "incomplete-verification-evidence",
        ] {
            for (banner, expected) in [
                (
                    "An action was blocked on this server based on a security policy!",
                    ObservedResult::SandboxDenied,
                ),
                (
                    "fatal: Could not resolve proxy",
                    ObservedResult::InfrastructureError,
                ),
            ] {
                let mut terminal = attempt_with_sabre_evidence("");
                terminal.outcome = "ERROR".into();
                terminal.error_kind = Some(error.into());
                terminal.stderr = banner.into();
                let attempts = [divergence.clone(), terminal];
                let bytes = serde_json::to_vec(&attempts).unwrap();
                for mode in ["verify", "replay"] {
                    let result = observed_result(mode, "ERROR", &attempts, Some(error));
                    assert_eq!(result, Some(expected), "{mode}, {error}, {banner}");
                    assert_eq!(
                        failure_class("ERROR", result, Some(error)),
                        Some(FailureClass::UnderstoodInfrastructureFailure)
                    );
                }
                assert_eq!(serde_json::to_vec(&attempts).unwrap(), bytes);
            }
        }
    }

    #[test]
    fn framework_serializes_environmental_results_from_captured_output() {
        let mut sandbox_denied = attempt_with_sabre_evidence("");
        sandbox_denied.outcome = "ERROR".into();
        sandbox_denied.error_kind = Some("incomplete-verification-evidence".into());
        sandbox_denied.stderr =
            "An action was blocked on this server based on a security policy!\n\
             Enforcer: FS, Reason: FILE_OPEN\n"
                .into();
        let sandbox_result = observed_result(
            "verify",
            &sandbox_denied.outcome,
            std::slice::from_ref(&sandbox_denied),
            sandbox_denied.error_kind.as_deref(),
        );
        assert_eq!(sandbox_result, Some(ObservedResult::SandboxDenied));
        assert_eq!(
            failure_class(
                &sandbox_denied.outcome,
                sandbox_result,
                sandbox_denied.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut proxy_denied = sandbox_denied.clone();
        proxy_denied.stderr = "fatal: unable to access repository: Could not resolve proxy".into();
        let infrastructure_result = observed_result(
            "verify",
            &proxy_denied.outcome,
            std::slice::from_ref(&proxy_denied),
            proxy_denied.error_kind.as_deref(),
        );
        assert_eq!(
            infrastructure_result,
            Some(ObservedResult::InfrastructureError)
        );
        assert_eq!(
            failure_class(
                &proxy_denied.outcome,
                infrastructure_result,
                proxy_denied.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodInfrastructureFailure)
        );

        let mut ordinary = sandbox_denied;
        ordinary.outcome = "FAIL".into();
        ordinary.error_kind = None;
        ordinary.stderr = "ordinary guest assertion failed".into();
        let ordinary_result = observed_result(
            "verify",
            &ordinary.outcome,
            std::slice::from_ref(&ordinary),
            ordinary.error_kind.as_deref(),
        );
        assert_eq!(ordinary_result, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(
                &ordinary.outcome,
                ordinary_result,
                ordinary.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );
    }

    #[test]
    fn sabre_path_evidence_requires_every_execution_and_no_fallback() {
        let clean = r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}"#;
        let complete = attempt_with_sabre_evidence(&format!("{clean}\n{clean}\n"));
        assert_eq!(
            summarize_sabre_path_evidence(std::slice::from_ref(&complete))
                .unwrap()
                .unwrap()["eligible"],
            true
        );

        let mut ptrace_reference = complete.clone();
        ptrace_reference.index = "parity-reference".into();
        ptrace_reference.argv[2] = "ptrace".into();
        ptrace_reference.sabre_path_evidence = None;
        ptrace_reference.sabre_path_evidence_sha256 = None;
        assert_eq!(
            summarize_sabre_path_evidence(&[complete, ptrace_reference])
                .unwrap()
                .unwrap()["eligible"],
            true,
            "a ptrace parity operand is not a missing SaBRe execution"
        );

        let short = attempt_with_sabre_evidence(&format!("{clean}\n"));
        let summary = summarize_sabre_path_evidence(std::slice::from_ref(&short))
            .unwrap()
            .unwrap();
        assert_eq!(summary["complete"], false);
        assert_eq!(summary["eligible"], false);

        let malformed = attempt_with_sabre_evidence(
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":"0","trusted_shared_object_sites":"0","trusted_shared_objects":"none"}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":"0","trusted_shared_object_sites":"0","trusted_shared_objects":"none"}
"#,
        );
        assert!(summarize_sabre_path_evidence(&[malformed]).is_err());

        let unknown = attempt_with_sabre_evidence(
            r#"{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[],"unexpected":0}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        let error = summarize_sabre_path_evidence(&[unknown]).unwrap_err();
        assert!(error.contains("unknown field `unexpected`"), "{error}");

        let missing = attempt_with_sabre_evidence(
            r#"{"schema":1,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        let error = summarize_sabre_path_evidence(&[missing]).unwrap_err();
        assert!(
            error.contains("missing field `guest_rpc_observed`"),
            "{error}"
        );

        let future = attempt_with_sabre_evidence(
            r#"{"schema":2,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[],"future_field":"value"}
{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}
"#,
        );
        assert_eq!(
            summarize_sabre_path_evidence(&[future]).unwrap_err(),
            "invalid SaBRe path-evidence record: schema must be 1"
        );
    }

    #[test]
    fn normalized_entropy_detects_a_narrowed_two_class_sweep() {
        let balanced = diversity_evidence(
            &["a".into(), "a".into(), "b".into(), "b".into()],
            Some(2),
            2,
            Some(0.8),
        );
        let narrowed = diversity_evidence(
            &["a".into(), "a".into(), "a".into(), "b".into()],
            Some(2),
            2,
            Some(0.8),
        );
        assert!(balanced["normalized_entropy"].as_f64().unwrap() > 0.99);
        assert!(narrowed["normalized_entropy"].as_f64().unwrap() < 0.82);
    }

    fn canonical_verification_report() -> VerificationReport {
        VerificationReport {
            verified: true,
            bitwise_parity: true,
            verdict: Verdict::Matched,
            no_result_reason: None,
            infrastructure_error: None,
            comparison: Some(crate::canonical_verdict::ComparisonReport {
                strictness: crate::canonical_verdict::LogCompareStrictness::Canonical,
                display_name: Some("BitwiseInfoV1".into()),
                compare_logs: true,
                compare_io_buffers: Some(true),
                log_scope: Some(crate::canonical_verdict::ComparedLogScope::Info),
                record_envelope: crate::canonical_verdict::RecordEnvelopeReport::AllRecordsV1,
                virtualize_time: Some(true),
                strip_lines: Some(false),
                canonicalize_addresses: Some(true),
                full_trace: Some(true),
                exact_remainder: Some(true),
                stripped_prefixes: Some(vec!["real-wall-clock-prefix/v1".into()]),
                canonicalizations: Some(vec!["host-address-to-first-appearance-ordinal/v1".into()]),
                ignore_lines: Some(false),
                skip_commit: Some(false),
                skip_detlog: Some(false),
            }),
            compared_log_messages: Some(crate::canonical_verdict::ComparedLogMessages {
                left: 1,
                right: 1,
            }),
            compared_outputs: Some(crate::canonical_verdict::ComparedOutputs {
                left: crate::canonical_verdict::ComparedOutput {
                    exit_code: Some(7),
                    ..parity_fixture_output()
                },
                right: crate::canonical_verdict::ComparedOutput {
                    exit_code: Some(7),
                    ..parity_fixture_output()
                },
            }),
            dbt_counted_branches: None,
            runtime: None,
            guest_exit_code: Some(7),
            guest_signal: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
        }
    }

    fn parity_fixture_output() -> crate::canonical_verdict::ComparedOutput {
        crate::canonical_verdict::ComparedOutput {
            exit_code: Some(0),
            signal: None,
            stdout_sha256: "a".repeat(64),
            stdout_bytes: 0,
            stderr_sha256: "b".repeat(64),
            stderr_bytes: 0,
        }
    }

    fn parity_fixture_verification(verdict: Verdict) -> VerificationReport {
        let mut report = canonical_verification_report();
        let output = parity_fixture_output();
        report.guest_exit_code = Some(0);
        report.compared_outputs = Some(crate::canonical_verdict::ComparedOutputs {
            left: output.clone(),
            right: output,
        });
        if verdict == Verdict::Diverged {
            report.verified = false;
            report.bitwise_parity = false;
            report.verdict = Verdict::Diverged;
            report.first_divergent_scheduler_turn = Some(7);
            report.first_divergent_virtual_nanoseconds = Some(18);
            report.first_divergent_record = Some(2);
            report.first_divergent_syscall = Some(1);
            report.first_divergent_left_message = Some("candidate-left".into());
            report.first_divergent_right_message = Some("candidate-right".into());
        }
        report
    }

    /// Run one verify cell through the production `run_cell` path against a
    /// fixture Hermit. The fixture records each invocation. It also records any
    /// `log-diff` call and fails it, so a reintroduced cross-backend comparison
    /// cannot pass silently.
    ///
    /// `verdict` is the fixture's own same-backend strict-verification verdict.
    /// A `Diverged` fixture writes a diverged report and exits 1, the shape a
    /// real `hermit run --verify` produces when its two runs differ.
    fn run_production_verify_fixture(
        backend: &str,
        verdict: Verdict,
    ) -> (CellResult, String, bool) {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-production-verify-{}-{:?}-{backend}-{verdict:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("verification.json"),
            serde_json::to_vec(&parity_fixture_verification(verdict)).unwrap(),
        )
        .unwrap();
        let exit_status = if verdict == Verdict::Diverged {
            "1"
        } else {
            "0"
        };
        fs::write(root.join("exit-status"), exit_status).unwrap();

        let hermit = root.join("hermit");
        fs::write(
            &hermit,
            r#"#!/bin/sh
set -eu
if [ "${1-}" = "log-diff" ]; then
  printf 'log-diff\n' >> "$PWD/invocations"
  exit 3
fi

backend=
verdict=
logdir=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --backend) backend=$2; shift 2 ;;
    --verify-json) verdict=$2; shift 2 ;;
    --verify-log-dir) logdir=$2; shift 2 ;;
    *) shift ;;
  esac
done
printf 'run:%s\n' "$backend" >> "$PWD/invocations"

# This log differs from anything a ptrace run of the same guest would write.
# Before https://github.com/rrnewton/hermit/issues/3301 that difference was a
# ParityFailure; now nothing compares it with another backend.
mkdir -p "$logdir"
printf 'INFO detcore: %s-only\n' "$backend" > "$logdir/run1_log_fixture.log"

if [ -n "${HERMIT_SABRE_PATH_EVIDENCE-}" ]; then
  evidence='{"schema":1,"guest_rpc_observed":true,"ptrace_fallback_sites":0,"trusted_shared_object_sites":0,"trusted_shared_objects":[]}'
  printf '%s\n%s\n' "$evidence" "$evidence" > "$HERMIT_SABRE_PATH_EVIDENCE"
fi

cp "$PWD/verification.json" "$verdict"
exit "$(cat "$PWD/exit-status")"
"#,
        )
        .unwrap();
        fs::set_permissions(&hermit, fs::Permissions::from_mode(0o755)).unwrap();

        let mut cell = ptrace_cell("verify");
        cell.test.id = "fixture/ordinary-verify-production".into();
        cell.id.test = cell.test.id.clone();
        cell.id.backend = Some(backend.into());
        cell.enabled = false;
        cell.cpu_timeout_seconds = 5;
        cell.timeout_seconds = 10;
        let mut context = run_context(&root);
        context.hermit_bin = hermit;
        context.run_verify_strict = true;
        context.keep_logs = true;

        let result = run_cell(&context, &cell).unwrap();
        assert_cpu_source_roundtrip(&result);
        let invocations = fs::read_to_string(root.join("invocations")).unwrap_or_default();
        let retained_parity_report = std::fs::read_dir(&result.artifact_dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("backend-parity")
            });
        fs::remove_dir_all(root).unwrap();
        (result, invocations, retained_parity_report)
    }

    #[test]
    fn selected_portable_parity_candidates_preserve_identical_guest_arguments() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        // The tests folded from the retired backend-parity-c bucket into
        // c-programs (https://github.com/rrnewton/hermit/issues/3301, slice S6).
        let folded = crate::retired_ids::RetiredIds::load(&root)
            .unwrap()
            .successors_of("backend-parity-c")
            .unwrap();
        let cells = manifests
            .select(&Selection {
                lane: Some("portable".into()),
                category: Some("c-programs".into()),
                population: Some(Population::Required),
                ..Selection::default()
            })
            .unwrap();
        let candidates = cells
            .iter()
            .filter(|cell| folded.contains(&cell.id.test))
            .filter(|cell| cell.id.mode == "verify" && cell.id.backend.as_deref() != Some("ptrace"))
            .collect::<Vec<_>>();
        // Pin the selected population and per-backend split, and check every
        // candidate's arguments.
        // S13 of https://github.com/rrnewton/hermit/issues/3301 enabled dbt on
        // pid-probe, which adds the one portable dbt candidate.
        assert_eq!(candidates.len(), 174);
        let mut by_backend = BTreeMap::new();
        for cell in &candidates {
            *by_backend
                .entry(cell.id.backend.as_deref().unwrap())
                .or_insert(0) += 1;
        }
        assert_eq!(
            by_backend,
            BTreeMap::from([("dbt", 1), ("kvm", 75), ("liteinst", 97), ("sabre", 1)])
        );
        assert!(
            candidates
                .iter()
                .any(|cell| cell.id.test == "c-programs/readdir-order-identity"
                    && cell.id.backend.as_deref() == Some("kvm"))
        );
        for cell in candidates {
            let mode = &cell.test.modes["verify"];
            let candidate = cell.id.backend.as_deref().unwrap();
            assert!(
                mode.backends_enabled
                    .iter()
                    .any(|backend| backend == "ptrace")
            );
            assert_eq!(
                mode.guest_args.get(candidate).cloned().unwrap_or_default(),
                mode.guest_args.get("ptrace").cloned().unwrap_or_default(),
                "{}",
                cell.id.test
            );
            if cell.id.test == "c-programs/readdir-order-identity" {
                assert_eq!(mode.guest_args[candidate], ["--require-small-determinized"]);
                assert_eq!(mode.guest_args["ptrace"], ["--require-small-determinized"]);
            }
        }
    }

    /// The kvm, liteinst and sabre candidates pinned above (75, 97 and 1) used to
    /// add a ptrace reference run and a `hermit log-diff` comparison, and the
    /// comparison could overwrite their outcome. Since
    /// https://github.com/rrnewton/hermit/issues/3301 each one runs only its own
    /// backend's strict verification, even when its log differs from ptrace's.
    #[test]
    fn non_ptrace_verify_cells_run_only_their_own_backend() {
        for backend in ["kvm", "liteinst", "sabre"] {
            let (result, invocations, retained_parity_report) =
                run_production_verify_fixture(backend, Verdict::Matched);
            assert_eq!(result.outcome, "PASS", "{backend}: {result:#?}");
            assert_eq!(result.result, Some(ObservedResult::Pass), "{backend}");
            assert_eq!(result.failure_class, None, "{backend}");
            assert_eq!(result.error_kind, None, "{backend}");
            assert!(result.backend_parity.is_none(), "{backend}");
            assert!(!retained_parity_report, "{backend}");
            assert_eq!(result.attempts.len(), 1, "{backend}");
            assert_eq!(result.attempts[0].index, "1", "{backend}");
            assert_eq!(
                verification_verdict(&result.attempts[0]),
                Some(Verdict::Matched),
                "{backend}"
            );
            assert_eq!(invocations, format!("run:{backend}\n"));
            let observations = &result.cpu_observations.as_ref().unwrap().invocations;
            assert_eq!(observations.len(), 1, "{backend}: {observations:#?}");
            assert!(
                matches!(
                    &observations[0].role,
                    InvocationRole::Execution { attempt_index, .. } if attempt_index == "1"
                ),
                "{backend}: {observations:#?}"
            );
            if backend == "sabre" {
                assert_eq!(result.execution_path.as_ref().unwrap()["eligible"], true);
            }
        }
    }

    /// The failing half of the test above. Before
    /// https://github.com/rrnewton/hermit/issues/3301 a non-ptrace cell whose
    /// own strict verification diverged was a determinism failure, and a SaBRe
    /// cell kept that result only while its path evidence was eligible. Removing
    /// the ptrace reference run must not change either fact: the cell fails as a
    /// product determinism failure, keeps the divergence position its own report
    /// located, and still runs its backend exactly once.
    #[test]
    fn non_ptrace_verify_cells_fail_on_their_own_divergence() {
        for backend in ["kvm", "liteinst", "sabre"] {
            let (result, invocations, retained_parity_report) =
                run_production_verify_fixture(backend, Verdict::Diverged);
            assert_eq!(result.outcome, "FAIL", "{backend}: {result:#?}");
            assert_eq!(
                result.result,
                Some(ObservedResult::DeterminismFailure),
                "{backend}"
            );
            assert_eq!(
                result.failure_class,
                Some(FailureClass::ProductFailure),
                "{backend}"
            );
            assert_eq!(result.error_kind, None, "{backend}");
            assert!(result.backend_parity.is_none(), "{backend}");
            assert!(!retained_parity_report, "{backend}");
            assert_eq!(result.attempts.len(), 1, "{backend}");
            assert_eq!(
                verification_verdict(&result.attempts[0]),
                Some(Verdict::Diverged),
                "{backend}"
            );
            assert_eq!(invocations, format!("run:{backend}\n"));
            assert_eq!(result.first_divergent_scheduler_turn, Some(7), "{backend}");
            assert_eq!(
                result.first_divergent_virtual_nanoseconds,
                Some(18),
                "{backend}"
            );
            assert_eq!(result.first_divergent_record, Some(2), "{backend}");
            assert_eq!(result.first_divergent_syscall, Some(1), "{backend}");
            assert_eq!(
                result.first_divergent_left_message.as_deref(),
                Some("candidate-left"),
                "{backend}"
            );
            assert_eq!(
                result.first_divergent_right_message.as_deref(),
                Some("candidate-right"),
                "{backend}"
            );
            if backend == "sabre" {
                assert_eq!(result.execution_path.as_ref().unwrap()["eligible"], true);
            }
        }
    }

    fn nonzero_with_canonical_receipt(mode: &str) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-status-bracket-{}-{mode}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let report = serde_json::to_string(&canonical_verification_report()).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/status".into(),
                mode: mode.into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf %s \"$1\" > \"$2\"; exit 7".into(),
                "sh".into(),
                report,
                verdict.to_string_lossy().into_owned(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            expected_guest_exit: None,
            expected_stdout: None,
            expected_stdout_contains: None,
            comparator: Comparator::Strict,
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
            normalize_ptrace_golden: false,
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    /// Build one attempt from a real subprocess, so this exercises the same
    /// classification path used by an actual manifest sweep.
    fn attempt_from_script(backend: &str, script: &str, report: Option<&str>) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-backend-availability-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            script.to_string(),
            "sh".to_string(),
        ];
        argv.push(report.unwrap_or("").to_string());
        argv.push(verdict.to_string_lossy().into_owned());
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/backend-availability".into(),
                mode: "verify".into(),
                backend: Some(backend.into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv,
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 10,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            expected_guest_exit: None,
            expected_stdout: None,
            expected_stdout_contains: None,
            comparator: Comparator::Strict,
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
            normalize_ptrace_golden: false,
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn skid_overshoot_receipt_is_an_infrastructure_error_with_comparison_evidence() {
        let mut report = canonical_verification_report();
        report.verified = false;
        report.bitwise_parity = false;
        report.verdict = Verdict::InfrastructureError;
        report.infrastructure_error = Some(InfrastructureError::SkidOvershoot { count: 2 });
        let report = serde_json::to_string(&report).unwrap();

        let result = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; exit 122",
            Some(&report),
        );

        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert_eq!(result.status, Some(122));
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("2 HERMIT_SKID_OVERSHOOT")),
            "infrastructure error must retain the recorded count: {result:?}"
        );
        assert!(
            result
                .verification_report
                .as_deref()
                .is_some_and(|json| json.contains("\"comparison\":{")),
            "infrastructure error must retain the completed comparison: {result:?}"
        );

        let mut before_comparison = canonical_verification_report();
        before_comparison.verified = false;
        before_comparison.bitwise_parity = false;
        before_comparison.verdict = Verdict::InfrastructureError;
        before_comparison.infrastructure_error =
            Some(InfrastructureError::SkidOvershoot { count: 1 });
        before_comparison.comparison = None;
        before_comparison.compared_log_messages = None;
        let before_comparison = serde_json::to_string(&before_comparison).unwrap();
        let result = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; exit 122",
            Some(&before_comparison),
        );
        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("1 HERMIT_SKID_OVERSHOOT")),
            "comparison-null infrastructure error must retain its cause: {result:?}"
        );
    }

    /// A backend this runner could not start and a backend that ran but recorded
    /// no comparison must stay visible while carrying different error kinds.
    ///
    /// The old shared incomplete-verification-evidence kind turned an unstaged
    /// backend into an apparent product defect. Classifying every nonzero exit
    /// as unavailable would be the same bug with the sign flipped, so this
    /// bracket exercises both directions through real subprocesses.
    #[test]
    fn an_unavailable_backend_is_not_reported_as_a_silent_one() {
        let no_result = serde_json::to_string(&VerificationReport::no_result()).unwrap();
        let unavailable = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: HERMIT_SABRE_BINARY=/nonexistent/sabre is not an executable file' >&2; exit 1",
            Some(&no_result),
        );
        let silent = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; exit 0",
            Some(&no_result),
        );

        assert_eq!(unavailable.outcome, "ERROR");
        assert_eq!(silent.outcome, "ERROR");
        assert_eq!(
            failure_class(
                &unavailable.outcome,
                observed_result(
                    "verify",
                    &unavailable.outcome,
                    std::slice::from_ref(&unavailable),
                    unavailable.error_kind.as_deref()
                ),
                unavailable.error_kind.as_deref()
            ),
            Some(FailureClass::UnderstoodPrerequisiteFailure)
        );
        assert_eq!(
            failure_class(
                &silent.outcome,
                observed_result(
                    "verify",
                    &silent.outcome,
                    std::slice::from_ref(&silent),
                    silent.error_kind.as_deref()
                ),
                silent.error_kind.as_deref()
            ),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            unavailable.error_kind.as_deref(),
            Some("backend-unavailable"),
            "an unrunnable backend must carry its own kind: {:?}",
            unavailable.reason
        );
        assert_eq!(
            silent.error_kind.as_deref(),
            Some("incomplete-verification-evidence"),
            "a backend that ran and recorded nothing keeps the evidence kind: {:?}",
            silent.reason
        );
        assert_ne!(unavailable.error_kind, silent.error_kind);

        let unavailable_reason = unavailable.reason.clone().expect("reason");
        let silent_reason = silent.reason.clone().expect("reason");
        assert!(
            unavailable_reason.contains("backend unavailable on this runner")
                && unavailable_reason.contains("sabre"),
            "unavailable reason must name the backend and environment: {unavailable_reason}"
        );
        assert!(
            !unavailable_reason.contains("verification report is missing"),
            "the old wording pointed at a missing file, not the backend: {unavailable_reason}"
        );
        assert!(
            silent_reason.contains("no_result"),
            "silent reason must still name the producer state: {silent_reason}"
        );
        assert_ne!(unavailable_reason, silent_reason);
    }

    #[test]
    fn backend_unavailable_requires_the_requested_backend_and_empty_stdout() {
        let no_result = serde_json::to_string(&VerificationReport::no_result()).unwrap();
        let wrong_backend = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=dbt' \
             'Error: backend \x60dbt\x60 is unavailable: no SDK' >&2; exit 7",
            Some(&no_result),
        );
        let guest_output = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; printf 'guest-started\\n'; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: spoofed' >&2; exit 8",
            Some(&no_result),
        );

        for result in [wrong_backend, guest_output] {
            assert_eq!(result.outcome, "ERROR", "unexpected result: {result:?}");
            assert_eq!(
                observed_result(
                    "verify",
                    &result.outcome,
                    std::slice::from_ref(&result),
                    result.error_kind.as_deref()
                ),
                None,
                "mismatched producer evidence must not manufacture a product result: {result:?}"
            );
            assert_eq!(
                failure_class(&result.outcome, None, result.error_kind.as_deref()),
                Some(FailureClass::NoResult),
                "mismatched producer evidence must remain no-result: {result:?}"
            );
            assert_eq!(
                result.error_kind.as_deref(),
                Some("invalid-backend-evidence"),
                "unexpected result: {result:?}"
            );
            assert!(
                result
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("does not match this attempt")),
                "mismatched producer evidence must name the mismatch: {result:?}"
            );
        }

        let mut unspecified = VerificationReport::no_result();
        unspecified.no_result_reason = None;
        let unspecified = serde_json::to_string(&unspecified).unwrap();
        let ordinary_failure = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; printf 'guest-started\\n'; exit 8",
            Some(&unspecified),
        );
        assert_eq!(ordinary_failure.outcome, "FAIL");
        let observed = observed_result(
            "verify",
            &ordinary_failure.outcome,
            std::slice::from_ref(&ordinary_failure),
            ordinary_failure.error_kind.as_deref(),
        );
        assert_eq!(observed, Some(ObservedResult::CrashError));
        assert_eq!(
            failure_class(
                &ordinary_failure.outcome,
                observed,
                ordinary_failure.error_kind.as_deref(),
            ),
            Some(FailureClass::ProductFailure),
            "an ordinary process failure without contradictory producer evidence remains product-attributed"
        );
    }

    #[test]
    fn backend_unavailable_survives_an_unreadable_current_report() {
        let unavailable = attempt_from_script(
            "sabre",
            "printf %s \"$1\" > \"$2\"; \
             printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre' \
             'Error: backend \x60sabre\x60 is unavailable: no staged runtime' >&2; exit 1",
            Some("{"),
        );

        assert_eq!(unavailable.outcome, "ERROR");
        assert_eq!(
            unavailable.error_kind.as_deref(),
            Some("backend-unavailable")
        );
        assert!(
            unavailable
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no staged runtime")),
            "unreadable evidence must not overwrite the pre-guest refusal: {unavailable:?}"
        );
    }

    #[test]
    fn launch_refusal_requires_the_producer_class_line() {
        let no_result = serde_json::to_string(&VerificationReport::no_result()).unwrap();
        let typed = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=guest-program-not-found' \
             'Error: Program /missing does not exist' >&2; exit 127",
            Some(&no_result),
        );
        let prose_only = attempt_from_script(
            "ptrace",
            "printf %s \"$1\" > \"$2\"; printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=cli-error' \
             'Error: Program /missing does not exist' >&2; exit 127",
            Some(&no_result),
        );

        assert_eq!(typed.error_kind.as_deref(), Some("guest-launch-refused"));
        assert_eq!(typed.outcome, "ERROR");
        assert_eq!(
            prose_only.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(prose_only.outcome, "ERROR");
        assert_eq!(
            failure_class(
                &prose_only.outcome,
                observed_result(
                    "verify",
                    &prose_only.outcome,
                    std::slice::from_ref(&prose_only),
                    prose_only.error_kind.as_deref()
                ),
                prose_only.error_kind.as_deref(),
            ),
            Some(FailureClass::NoResult),
            "English launch prose without the producer class must remain no-result"
        );
    }
    #[test]
    fn cli_error_preserves_the_producer_cause_without_inventing_one() {
        let vector_13 = attempt_from_script(
            "kvm",
            "printf '%s\\n' \
             'HERMIT_INTERNAL_FAILURE class=cli-error' \
             'Error: KVM guest execution failed: guest exception vector 13' >&2; exit 1",
            None,
        );
        assert_eq!(vector_13.outcome, "ERROR");
        assert_eq!(
            vector_13.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(
            vector_13.reason.as_deref(),
            Some("KVM guest execution failed: guest exception vector 13")
        );

        let no_detail = attempt_from_script(
            "kvm",
            "printf '%s\\n' 'HERMIT_INTERNAL_FAILURE class=cli-error' >&2; exit 1",
            None,
        );
        assert_eq!(no_detail.outcome, "ERROR");
        assert_eq!(no_detail.reason, None, "absence must remain honest absence");
    }

    #[test]
    fn canonical_receipt_does_not_erase_process_failure_except_for_chaos() {
        let verify = nonzero_with_canonical_receipt("verify");
        assert_eq!(verify.outcome, "FAIL");
        assert_eq!(verify.status, Some(7));

        let replay = nonzero_with_canonical_receipt("replay");
        assert_eq!(replay.outcome, "FAIL");
        assert_eq!(replay.status, Some(7));

        let chaos = nonzero_with_canonical_receipt("chaos");
        assert_eq!(chaos.outcome, "PASS");
        assert_eq!(chaos.status, Some(7));
    }

    fn expected_exit(code: Option<i32>, signal: Option<i32>) -> ExpectedGuestExit {
        ExpectedGuestExit {
            code,
            signal,
            reason: "the fixture guest fails on purpose".into(),
        }
    }

    /// Run a fake Hermit that writes a matched canonical report carrying the
    /// given guest disposition, then ends with `ending` (a shell statement).
    fn attempt_with_expected_exit(
        expected: ExpectedGuestExit,
        guest_exit_code: Option<i32>,
        guest_signal: Option<i32>,
        ending: &str,
    ) -> AttemptResult {
        attempt_with_expected_exit_report(
            expected,
            expected_exit_report(guest_exit_code, guest_signal),
            ending,
            5,
        )
    }

    /// A matched canonical report carrying the given guest disposition.
    fn expected_exit_report(
        guest_exit_code: Option<i32>,
        guest_signal: Option<i32>,
    ) -> VerificationReport {
        let mut report = canonical_verification_report();
        report.guest_exit_code = guest_exit_code;
        report.guest_signal = guest_signal;
        let outputs = report.compared_outputs.as_mut().unwrap();
        for output in [&mut outputs.left, &mut outputs.right] {
            output.exit_code = guest_exit_code;
            output.signal = guest_signal;
        }
        report
    }

    /// Run a fake Hermit that writes `report`, then ends with `ending`, under
    /// a wall backstop of `timeout_seconds`.
    fn attempt_with_expected_exit_report(
        expected: ExpectedGuestExit,
        report: VerificationReport,
        ending: &str,
        timeout_seconds: u64,
    ) -> AttemptResult {
        attempt_with_report(
            Some(expected),
            Comparator::Strict,
            report,
            ending,
            timeout_seconds,
        )
    }

    /// [`attempt_with_expected_exit_report`] for any declared exit and comparator.
    fn attempt_with_report(
        expected: Option<ExpectedGuestExit>,
        comparator: Comparator,
        report: VerificationReport,
        ending: &str,
        timeout_seconds: u64,
    ) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-expected-exit-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/expected-exit".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("printf %s \"$1\" > \"$2\"; {ending}"),
                "sh".into(),
                serde_json::to_string(&report).unwrap(),
                verdict.to_string_lossy().into_owned(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            expected_guest_exit: expected,
            expected_stdout: None,
            expected_stdout_contains: None,
            comparator,
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
            normalize_ptrace_golden: false,
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn expected_guest_exit_passes_only_the_exact_disposition() {
        let exact =
            attempt_with_expected_exit(expected_exit(Some(7), None), Some(7), None, "exit 7");
        assert_eq!(exact.outcome, "PASS", "{:?}", exact.reason);

        let other_code =
            attempt_with_expected_exit(expected_exit(Some(7), None), Some(6), None, "exit 6");
        assert_eq!(
            other_code.outcome, "FAIL",
            "{:?} {:?}",
            other_code.error_kind, other_code.reason
        );
        assert_eq!(
            other_code.reason.as_deref(),
            Some("guest ended with exit code 6, but the manifest expects exit code 7")
        );

        // The report and Hermit's own status must agree; a report alone is
        // not enough to call the attempt a pass.
        let hermit_disagrees =
            attempt_with_expected_exit(expected_exit(Some(7), None), Some(7), None, "exit 0");
        assert_eq!(hermit_disagrees.outcome, "FAIL");
        assert_eq!(
            hermit_disagrees.reason.as_deref(),
            Some(
                "guest ended with exit code 7, but hermit exited with status 0, which does not report exit code 7"
            )
        );

        // Fixtures that make the fake Hermit die by a signal use signals that
        // never dump core. A core-dumping signal hands the dump to the host's
        // core_pattern helper, which ignores RLIMIT_CORE when it is a pipe and
        // took from 0.5 to over 5 seconds on a loaded host, past this
        // fixture's 5-second backstop.
        let raised = attempt_with_expected_exit(
            expected_exit(None, Some(15)),
            None,
            Some(15),
            "kill -TERM $$",
        );
        assert_eq!(raised.outcome, "PASS", "{:?}", raised.reason);
        assert_eq!(raised.signal, Some(15));

        let shell_status =
            attempt_with_expected_exit(expected_exit(None, Some(11)), None, Some(11), "exit 139");
        assert_eq!(shell_status.outcome, "PASS", "{:?}", shell_status.reason);

        // An exit code of 139 is not a SIGSEGV death, even though Hermit's
        // process status is the same number.
        let code_not_signal =
            attempt_with_expected_exit(expected_exit(None, Some(11)), Some(139), None, "exit 139");
        assert_eq!(code_not_signal.outcome, "FAIL");
        assert_eq!(
            code_not_signal.reason.as_deref(),
            Some("guest ended with exit code 139, but the manifest expects signal 11")
        );

        let other_signal = attempt_with_expected_exit(
            expected_exit(None, Some(15)),
            None,
            Some(10),
            "kill -USR1 $$",
        );
        assert_eq!(other_signal.outcome, "FAIL");
        assert_eq!(
            other_signal.reason.as_deref(),
            Some("guest ended with signal 10, but the manifest expects signal 15")
        );

        // A matching signal in the report is not enough when Hermit's own
        // status reports a success, a different signal, or 128 plus a
        // different signal number.
        for (ending, hermit) in [
            ("exit 0", "exited with status 0"),
            ("kill -USR1 $$", "was killed by signal 10"),
            ("exit 138", "exited with status 138"),
        ] {
            let hermit_disagrees =
                attempt_with_expected_exit(expected_exit(None, Some(15)), None, Some(15), ending);
            assert_eq!(hermit_disagrees.outcome, "FAIL", "{ending}");
            assert_eq!(
                hermit_disagrees.reason,
                Some(format!(
                    "guest ended with signal 15, but hermit {hermit}, which does not report signal 15"
                )),
                "{ending}"
            );
        }

        let succeeded =
            attempt_with_expected_exit(expected_exit(Some(1), None), Some(0), None, "exit 0");
        assert_eq!(succeeded.outcome, "FAIL");

        // A matching exit code in the report is not enough when Hermit itself
        // was killed by a signal.
        let hermit_killed = attempt_with_expected_exit(
            expected_exit(Some(7), None),
            Some(7),
            None,
            "kill -TERM $$",
        );
        assert_eq!(hermit_killed.outcome, "FAIL");
        assert_eq!(
            hermit_killed.reason.as_deref(),
            Some(
                "guest ended with exit code 7, but hermit was killed by signal 15, which does not report exit code 7"
            )
        );
    }

    /// The expected disposition is checked only after the comparison has been
    /// admitted as canonical and matched. Each report below carries exactly
    /// the expected exit and Hermit exits with it, so only the canonical
    /// checks stand between the attempt and a pass.
    #[test]
    fn expected_guest_exit_cannot_pass_a_diverged_or_noncanonical_comparison() {
        let mut diverged = expected_exit_report(Some(7), None);
        diverged.verified = false;
        diverged.bitwise_parity = false;
        diverged.verdict = Verdict::Diverged;
        diverged.first_divergent_scheduler_turn = Some(4);
        diverged.first_divergent_virtual_nanoseconds = Some(7);
        diverged.first_divergent_record = Some(9);
        diverged.first_divergent_syscall = Some(2);
        diverged.first_divergent_left_message = Some("left".into());
        diverged.first_divergent_right_message = Some("right".into());
        let result =
            attempt_with_expected_exit_report(expected_exit(Some(7), None), diverged, "exit 7", 5);
        assert_eq!(result.outcome, "FAIL", "{:?}", result.reason);
        assert_eq!(
            result.reason.as_deref(),
            Some(
                "canonical verification did not match: verified=false verdict=diverged bitwise_parity=false"
            )
        );

        let mut stripped = expected_exit_report(Some(7), None);
        stripped.comparison.as_mut().unwrap().strictness =
            crate::canonical_verdict::LogCompareStrictness::Stripped;
        let result =
            attempt_with_expected_exit_report(expected_exit(Some(7), None), stripped, "exit 7", 5);
        assert_eq!(result.outcome, "ERROR", "{:?}", result.reason);
        assert_eq!(
            result.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(
            result.reason.as_deref(),
            Some(
                "verification did not compare canonical non-vacuous INFO evidence: strictness=stripped compare_logs=true record_envelope=all_records_v1 messages=1/1"
            )
        );
    }

    /// Run a real verify cell through `run_cell` against a fake Hermit that
    /// writes `report` and ends with `ending`, returning the serialized row.
    fn expected_exit_row(
        expected: Option<ExpectedGuestExit>,
        report: VerificationReport,
        ending: &str,
    ) -> (CellResult, serde_json::Value) {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-expected-exit-row-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("verification.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let hermit = root.join("hermit");
        fs::write(
            &hermit,
            format!(
                r#"#!/bin/sh
set -eu
verdict=
logdir=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --verify-json) verdict=$2; shift 2 ;;
    --verify-log-dir) logdir=$2; shift 2 ;;
    *) shift ;;
  esac
done
if [ -n "$logdir" ]; then
  mkdir -p "$logdir"
  printf 'INFO detcore: shared\n' > "$logdir/run1_log_fixture.log"
fi
cp "{}" "$verdict"
{ending}
"#,
                root.join("verification.json").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&hermit, fs::Permissions::from_mode(0o755)).unwrap();

        let mut cell = ptrace_cell("verify");
        cell.test.id = "fixture/expected-exit-row".into();
        cell.id.test = cell.test.id.clone();
        cell.timeout_seconds = 10;
        cell.cpu_timeout_seconds = 5;
        cell.test
            .modes
            .get_mut("verify")
            .unwrap()
            .expected_guest_exit = expected;
        let mut context = run_context(&root);
        context.hermit_bin = hermit;
        context.run_verify_strict = true;

        let result = run_cell(&context, &cell).unwrap();
        let row = serde_json::to_value(&result).unwrap();
        fs::remove_dir_all(root).unwrap();
        (result, row)
    }

    /// A verify cell that passes on its declared nonzero disposition must say
    /// so on its row. Without the declaration a reader of `results.jsonl` sees
    /// a matched attempt with a nonzero status and no authority for it, and
    /// must refuse the row -- which is what the series writer did to every
    /// validate batch that contained one of these cells.
    #[test]
    fn a_declared_expected_exit_is_carried_on_the_result_row() {
        let (code_result, code_row) = expected_exit_row(
            Some(expected_exit(Some(7), None)),
            expected_exit_report(Some(7), None),
            "exit 7",
        );
        assert_eq!(code_result.outcome, "PASS", "{:?}", code_result.reason);
        assert_eq!(code_result.attempts[0].status, Some(7));
        assert!(
            code_result.attempts[0]
                .argv
                .iter()
                .any(|arg| arg == "--verify-allow=failure")
        );
        assert_eq!(
            code_row["expected_guest_exit"],
            serde_json::json!({
                "code": 7,
                "signal": null,
                "reason": "the fixture guest fails on purpose",
            })
        );

        // The 128 + signo spelling of a signal death carries the declared
        // signal, not the status Hermit happened to use to report it.
        let (signal_result, signal_row) = expected_exit_row(
            Some(expected_exit(None, Some(11))),
            expected_exit_report(None, Some(11)),
            "exit 139",
        );
        assert_eq!(signal_result.outcome, "PASS", "{:?}", signal_result.reason);
        assert_eq!(signal_result.attempts[0].status, Some(139));
        assert_eq!(
            signal_row["expected_guest_exit"],
            serde_json::json!({
                "code": null,
                "signal": 11,
                "reason": "the fixture guest fails on purpose",
            })
        );
        let reread: CellResult = serde_json::from_value(signal_row).unwrap();
        assert_eq!(
            reread.expected_guest_exit,
            Some(expected_exit(None, Some(11)))
        );

        // A cell without a declaration keeps the historical row shape: the key
        // is absent, not null, and such a row still reads back.
        let (plain_result, plain_row) =
            expected_exit_row(None, canonical_verification_report(), "exit 0");
        assert_eq!(plain_result.outcome, "PASS", "{:?}", plain_result.reason);
        assert!(
            !plain_row
                .as_object()
                .unwrap()
                .contains_key("expected_guest_exit"),
            "{plain_row}"
        );
        let reread: CellResult = serde_json::from_value(plain_row).unwrap();
        assert_eq!(reread.expected_guest_exit, None);
    }

    /// Only a verify cell carries a declaration; the same recipe field on any
    /// other mode is not authority for a nonzero pass there.
    #[test]
    fn only_a_verify_cell_carries_its_declared_expected_exit() {
        let mut verify = ptrace_cell("verify");
        verify
            .test
            .modes
            .get_mut("verify")
            .unwrap()
            .expected_guest_exit = Some(expected_exit(Some(3), None));
        assert_eq!(
            cell_expected_guest_exit(&verify),
            Some(expected_exit(Some(3), None))
        );
        for mode in ["chaos", "replay", "naked"] {
            let mut other = ptrace_cell(mode);
            other.test.modes.get_mut(mode).unwrap().expected_guest_exit =
                Some(expected_exit(Some(3), None));
            assert_eq!(cell_expected_guest_exit(&other), None, "{mode}");
        }
        assert_eq!(cell_expected_guest_exit(&ptrace_cell("verify")), None);
    }

    /// A canonical matched report whose two runs printed `first` and
    /// `second` on stdout.
    fn stdout_report(first: &str, second: &str) -> VerificationReport {
        let mut report = canonical_verification_report();
        let outputs = report.compared_outputs.as_mut().unwrap();
        outputs.left.stdout_sha256 = hex_digest(first.as_bytes());
        outputs.left.stdout_bytes = first.len() as u64;
        outputs.right.stdout_sha256 = hex_digest(second.as_bytes());
        outputs.right.stdout_bytes = second.len() as u64;
        report
    }

    fn attempt_with_expected_stdout(
        expected: &str,
        report: VerificationReport,
        ending: &str,
    ) -> AttemptResult {
        attempt_with_stdout_assertions(Some(expected), None, report, "", ending)
    }

    /// Run a fake Hermit that writes `report` as its verdict and prints
    /// `captured` on stdout, against a dbt verify spec declaring `exact`
    /// and/or `contains`.
    fn attempt_with_stdout_assertions(
        exact: Option<&str>,
        contains: Option<&str>,
        report: VerificationReport,
        captured: &str,
        ending: &str,
    ) -> AttemptResult {
        let dir = std::env::temp_dir().join(format!(
            "hermit-runner-expected-stdout-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let verdict = dir.join("verdict.json");
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/expected-stdout".into(),
                mode: "verify".into(),
                backend: Some("dbt".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("printf %s \"$1\" > \"$2\"; printf %s \"$3\"; {ending}"),
                "sh".into(),
                serde_json::to_string(&report).unwrap(),
                verdict.to_string_lossy().into_owned(),
                captured.into(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            expected_guest_exit: None,
            expected_stdout: exact.map(str::to_string),
            expected_stdout_contains: contains.map(str::to_string),
            comparator: Comparator::Strict,
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
            normalize_ptrace_golden: false,
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    /// Two runs that agree with each other on the wrong bytes are a failure
    /// when the cell names the exact bytes.
    #[test]
    fn expected_stdout_passes_only_when_both_runs_print_it() {
        let golden = "hello world\n";
        let matched = attempt_with_expected_stdout(golden, stdout_report(golden, golden), "exit 0");
        assert_eq!(matched.outcome, "PASS", "{:?}", matched.reason);

        let golden_sha = hex_digest(golden.as_bytes());
        let other = "hello_world\n";
        let other_sha = hex_digest(other.as_bytes());
        let agreed_wrong =
            attempt_with_expected_stdout(golden, stdout_report(other, other), "exit 0");
        assert_eq!(agreed_wrong.outcome, "FAIL", "{:?}", agreed_wrong.reason);
        assert_eq!(agreed_wrong.error_kind, None);
        assert_eq!(
            agreed_wrong.reason,
            Some(format!(
                "verify first run stdout (12 bytes, sha256 {other_sha}) differs from the declared expected_stdout (12 bytes, sha256 {golden_sha})"
            ))
        );

        // A matched report whose two runs printed different bytes never
        // reaches the stdout check: the report reader refuses it first, and
        // the cell has no verdict rather than a pass.
        for report in [stdout_report(other, golden), stdout_report(golden, other)] {
            let result = attempt_with_expected_stdout(golden, report, "exit 0");
            assert_eq!(result.outcome, "ERROR", "{:?}", result.reason);
        }

        // An empty declaration is a real assertion, not an absent one.
        let empty = attempt_with_expected_stdout("", stdout_report("x", "x"), "exit 0");
        assert_eq!(empty.outcome, "FAIL", "{:?}", empty.reason);
        let silent = attempt_with_expected_stdout("", stdout_report("", ""), "exit 0");
        assert_eq!(silent.outcome, "PASS", "{:?}", silent.reason);
    }

    /// A report that omits the compared outputs cannot show the declared
    /// bytes. The report reader already refuses such a report, and the check
    /// itself also refuses to decide rather than pass.
    #[test]
    fn expected_stdout_without_compared_outputs_is_never_a_pass() {
        let mut report = stdout_report("ok\n", "ok\n");
        report.compared_outputs = None;
        assert_eq!(
            check_expected_stdout("ok\n", &report),
            Err(ExpectedStdoutError::Unevidenced(
                "verification report omitted compared_outputs, so the declared expected_stdout cannot be checked".into()
            ))
        );
        let result = attempt_with_expected_stdout("ok\n", report, "exit 0");
        assert_eq!(result.outcome, "ERROR", "{:?}", result.reason);
        assert_eq!(
            result.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
    }

    /// Each compared run is checked on its own, so a second run that alone
    /// printed the wrong bytes is named.
    #[test]
    fn expected_stdout_names_the_run_that_differs() {
        let good = stdout_report("ok\n", "ok\n");
        assert_eq!(check_expected_stdout("ok\n", &good), Ok(()));
        let second = stdout_report("ok\n", "no\n");
        let Err(ExpectedStdoutError::Mismatch(error)) = check_expected_stdout("ok\n", &second)
        else {
            panic!("second-run difference was not a mismatch");
        };
        assert!(
            error.starts_with("verify second run stdout (3 bytes"),
            "{error}"
        );
        // Equal length is not enough; the digest decides.
        let first = stdout_report("no\n", "ok\n");
        let Err(ExpectedStdoutError::Mismatch(error)) = check_expected_stdout("ok\n", &first)
        else {
            panic!("first-run difference was not a mismatch");
        };
        assert!(
            error.starts_with("verify first run stdout (3 bytes"),
            "{error}"
        );
    }

    /// Matching bytes never rescue a diverged comparison or a failed Hermit
    /// status: the stdout check is one more requirement, not a substitute.
    #[test]
    fn expected_stdout_cannot_pass_a_diverged_comparison_or_failed_status() {
        let mut diverged = stdout_report("ok\n", "ok\n");
        diverged.verified = false;
        diverged.bitwise_parity = false;
        diverged.verdict = Verdict::Diverged;
        diverged.first_divergent_record = Some(3);
        let result = attempt_with_expected_stdout("ok\n", diverged, "exit 0");
        assert_eq!(result.outcome, "FAIL", "{:?}", result.reason);
        assert_eq!(
            result.reason.as_deref(),
            Some(
                "canonical verification did not match: verified=false verdict=diverged bitwise_parity=false"
            )
        );

        let failed = attempt_with_expected_stdout("ok\n", stdout_report("ok\n", "ok\n"), "exit 3");
        assert_eq!(failed.outcome, "FAIL", "{:?}", failed.reason);
        assert_eq!(
            failed.reason.as_deref(),
            Some("verify exited with status 3")
        );
    }

    /// Only a verify cell reads its backend's declaration, and only its own
    /// backend's entry.
    #[test]
    fn only_a_verify_cell_reads_its_own_backends_expected_stdout() {
        let mut verify = ptrace_cell("verify");
        verify.test.modes.get_mut("verify").unwrap().expected_stdout = BTreeMap::from([
            ("ptrace".into(), "p\n".into()),
            ("dbt".into(), "d\n".into()),
        ]);
        assert_eq!(cell_expected_stdout(&verify).as_deref(), Some("p\n"));
        verify.id.backend = Some("dbt".into());
        assert_eq!(cell_expected_stdout(&verify).as_deref(), Some("d\n"));
        verify.id.backend = Some("kvm".into());
        assert_eq!(cell_expected_stdout(&verify), None);
        for mode in ["chaos", "replay", "naked"] {
            let mut other = ptrace_cell(mode);
            other.test.modes.get_mut(mode).unwrap().expected_stdout =
                BTreeMap::from([("ptrace".into(), "p\n".into())]);
            assert_eq!(cell_expected_stdout(&other), None, "{mode}");
        }
        assert_eq!(cell_expected_stdout(&ptrace_cell("verify")), None);
    }

    #[test]
    fn expected_stdout_declarations_are_verify_only_and_name_enabled_backends() {
        let enabled = BTreeSet::from(["ptrace", "dbt"]);
        let declared = BTreeMap::from([("dbt".to_string(), "ok\n".to_string())]);
        assert_eq!(
            validate_expected_stdout("b/t", "verify", &declared, &enabled),
            Ok(())
        );
        assert_eq!(
            validate_expected_stdout("b/t", "chaos", &BTreeMap::new(), &enabled),
            Ok(())
        );
        assert_eq!(
            validate_expected_stdout("b/t", "chaos", &declared, &enabled),
            Err("b/t: expected_stdout is supported only by verify mode".into())
        );
        let outside = BTreeMap::from([("kvm".to_string(), "ok\n".to_string())]);
        assert_eq!(
            validate_expected_stdout("b/t", "verify", &outside, &enabled),
            Err("b/t: verify expected_stdout names backend kvm outside backends_enabled".into())
        );
    }

    /// Agreeing runs pass a declared marker only when the captured stream
    /// both is the compared runs' stdout and contains the marker.
    #[test]
    fn expected_stdout_contains_requires_the_marker_in_the_compared_stdout() {
        let printed = "heap 0x555555559000 0x555555559000\n";
        let passed = attempt_with_stdout_assertions(
            None,
            Some("heap "),
            stdout_report(printed, printed),
            printed,
            "exit 0",
        );
        assert_eq!(passed.outcome, "PASS", "{:?}", passed.reason);

        let unmarked = "0x555555559000 0x555555559000\n";
        let unmarked_sha = hex_digest(unmarked.as_bytes());
        let missing = attempt_with_stdout_assertions(
            None,
            Some("heap "),
            stdout_report(unmarked, unmarked),
            unmarked,
            "exit 0",
        );
        assert_eq!(missing.outcome, "FAIL", "{:?}", missing.reason);
        assert_eq!(missing.error_kind, None);
        assert_eq!(
            missing.reason,
            Some(format!(
                "verify stdout ({} bytes, sha256 {unmarked_sha}) does not contain the declared expected_stdout_contains text \"heap \"",
                unmarked.len()
            ))
        );

        // A capture that is not the compared bytes proves nothing about
        // them, even when it happens to contain the marker.
        let foreign = attempt_with_stdout_assertions(
            None,
            Some("heap "),
            stdout_report(unmarked, unmarked),
            printed,
            "exit 0",
        );
        assert_eq!(foreign.outcome, "ERROR", "{:?}", foreign.reason);
        assert_eq!(
            foreign.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert!(
            foreign
                .reason
                .as_deref()
                .unwrap()
                .starts_with("captured verify stdout ("),
            "{:?}",
            foreign.reason
        );

        let mut omitted = stdout_report(printed, printed);
        omitted.compared_outputs = None;
        assert_eq!(
            check_expected_stdout_contains("heap ", printed.as_bytes(), &omitted),
            Err(ExpectedStdoutError::Unevidenced(
                "verification report omitted compared_outputs, so the declared expected_stdout_contains cannot be checked".into()
            ))
        );
        let second_differs = stdout_report(printed, unmarked);
        let Err(ExpectedStdoutError::Unevidenced(error)) =
            check_expected_stdout_contains("heap ", printed.as_bytes(), &second_differs)
        else {
            panic!("a capture unlike the second run must be unevidenced");
        };
        assert!(
            error.contains("is not the second compared run's stdout"),
            "{error}"
        );
    }

    /// A marker cannot rescue a diverged comparison or a failed exit.
    #[test]
    fn expected_stdout_contains_cannot_pass_a_diverged_or_failed_run() {
        let printed = "clock matrix success\n";
        let mut diverged = stdout_report(printed, printed);
        diverged.verified = false;
        diverged.bitwise_parity = false;
        diverged.verdict = Verdict::Diverged;
        diverged.first_divergent_record = Some(3);
        let result =
            attempt_with_stdout_assertions(None, Some("success"), diverged, printed, "exit 0");
        assert_eq!(result.outcome, "FAIL", "{:?}", result.reason);
        assert_eq!(
            result.reason.as_deref(),
            Some(
                "canonical verification did not match: verified=false verdict=diverged bitwise_parity=false"
            )
        );

        let failed = attempt_with_stdout_assertions(
            None,
            Some("success"),
            stdout_report(printed, printed),
            printed,
            "exit 3",
        );
        assert_eq!(failed.outcome, "FAIL", "{:?}", failed.reason);
        assert_eq!(
            failed.reason.as_deref(),
            Some("verify exited with status 3")
        );
    }

    #[test]
    fn expected_stdout_contains_declarations_are_verify_only_and_non_empty() {
        let enabled = BTreeSet::from(["ptrace", "dbt"]);
        let declared = BTreeMap::from([("dbt".to_string(), "heap ".to_string())]);
        assert_eq!(
            validate_expected_stdout_contains("b/t", "verify", &declared, &enabled),
            Ok(())
        );
        assert_eq!(
            validate_expected_stdout_contains("b/t", "replay", &declared, &enabled),
            Err("b/t: expected_stdout_contains is supported only by verify mode".into())
        );
        let outside = BTreeMap::from([("sabre".to_string(), "heap ".to_string())]);
        assert_eq!(
            validate_expected_stdout_contains("b/t", "verify", &outside, &enabled),
            Err(
                "b/t: verify expected_stdout_contains names backend sabre outside backends_enabled"
                    .into()
            )
        );
        let empty = BTreeMap::from([("ptrace".to_string(), String::new())]);
        assert_eq!(
            validate_expected_stdout_contains("b/t", "verify", &empty, &enabled),
            Err("b/t: verify expected_stdout_contains.ptrace must not be empty".into())
        );
    }

    /// A golden carrying the vDSO getrandom leg cannot be produced on a
    /// pre-6.11 kernel, so the manifest must declare that floor; the same
    /// golden with the token declared, and an unrelated golden without it,
    /// both load.
    #[test]
    fn a_vdso_getrandom_golden_must_declare_its_kernel_floor() {
        let golden = "getrandom[0]=aa\nvdso-getrandom[0]=bb\n".to_string();
        for key in ["expected_stdout", "expected_stdout_contains"] {
            let mut cell = ptrace_cell("verify");
            let recipe = cell.test.modes.get_mut("verify").unwrap();
            let table = BTreeMap::from([("ptrace".to_string(), golden.clone())]);
            if key == "expected_stdout" {
                recipe.expected_stdout = table;
            } else {
                recipe.expected_stdout_contains = table;
            }
            let undeclared = vec!["linux".to_string(), "cc".to_string()];
            assert_eq!(
                validate_golden_kernel_floor("b/t", &undeclared, &cell.test.modes),
                Err(format!(
                    "b/t: verify {key}.ptrace prints the vDSO getrandom leg, which only a \
                     Linux 6.11+ host exports; declare `vdso-getrandom` in requires"
                ))
            );
            let declared = vec!["linux".to_string(), "vdso-getrandom".to_string()];
            assert_eq!(
                validate_golden_kernel_floor("b/t", &declared, &cell.test.modes),
                Ok(())
            );
        }
        let mut cell = ptrace_cell("verify");
        cell.test.modes.get_mut("verify").unwrap().expected_stdout = BTreeMap::from([(
            "ptrace".to_string(),
            "vdso-getrandom unavailable\n".to_string(),
        )]);
        assert_eq!(
            validate_golden_kernel_floor("b/t", &[], &cell.test.modes),
            Ok(())
        );
        // The token is descriptive: it never withholds a cell.
        assert_eq!(requires_capability("vdso-getrandom"), Ok(None));
    }

    #[test]
    fn only_a_verify_cell_reads_its_own_backends_expected_stdout_contains() {
        let mut cell = ptrace_cell("verify");
        cell.test
            .modes
            .get_mut("verify")
            .unwrap()
            .expected_stdout_contains = BTreeMap::from([("ptrace".into(), "pid=".into())]);
        assert_eq!(
            cell_expected_stdout_contains(&cell).as_deref(),
            Some("pid=")
        );
        cell.id.backend = Some("dbt".into());
        assert_eq!(cell_expected_stdout_contains(&cell), None);
        let mut chaos = ptrace_cell("chaos");
        chaos
            .test
            .modes
            .get_mut("chaos")
            .unwrap()
            .expected_stdout_contains = BTreeMap::from([("ptrace".into(), "pid=".into())]);
        assert_eq!(cell_expected_stdout_contains(&chaos), None);
    }

    /// An attempt stopped by the wall backstop is never a pass, even when it
    /// left a matched canonical report of the expected exit and Hermit, on the
    /// backstop's SIGTERM, still exited with that code.
    #[test]
    fn expected_guest_exit_cannot_pass_a_timed_out_attempt() {
        let result = attempt_with_expected_exit_report(
            expected_exit(Some(7), None),
            expected_exit_report(Some(7), None),
            "trap 'exit 7' TERM; sleep 30 & wait",
            1,
        );
        assert!(result.timed_out, "{result:?}");
        assert_eq!(result.status, Some(7), "{result:?}");
        assert_eq!(result.outcome, "FAIL", "{:?}", result.reason);
        assert_eq!(result.error_kind.as_deref(), Some("wall-timeout"));
    }

    #[test]
    fn expected_guest_exit_validation_requires_one_exact_nonzero_disposition() {
        let check = |mode: &str, expected: ExpectedGuestExit| {
            validate_expected_guest_exit("fixture/test", mode, Some(&expected))
        };
        check("verify", expected_exit(Some(7), None)).unwrap();
        check("verify", expected_exit(None, Some(11))).unwrap();
        validate_expected_guest_exit("fixture/test", "verify", None).unwrap();

        for (expected, message) in [
            (
                expected_exit(Some(0), None),
                "code must be a nonzero exit status",
            ),
            (
                expected_exit(Some(256), None),
                "code must be a nonzero exit status",
            ),
            (
                expected_exit(None, Some(0)),
                "signal must be a signal number",
            ),
            (
                expected_exit(None, Some(65)),
                "signal must be a signal number",
            ),
            (
                expected_exit(Some(7), Some(11)),
                "exactly one of code or signal",
            ),
            (expected_exit(None, None), "exactly one of code or signal"),
        ] {
            let error = check("verify", expected).unwrap_err();
            assert!(error.contains(message), "{error}");
        }
        let mut blank = expected_exit(Some(7), None);
        blank.reason = "  ".into();
        assert!(
            check("verify", blank)
                .unwrap_err()
                .contains("reason must be substantive")
        );
        assert!(
            check("chaos", expected_exit(Some(7), None))
                .unwrap_err()
                .contains("supported only by verify mode")
        );
    }

    #[test]
    fn chaos_assertion_does_not_relabel_an_incomplete_population_as_a_product_failure() {
        let mut outcome = "ERROR".to_string();
        let mut reason = Some("seed-31 timed out before producing a comparison".to_string());
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            Some("cpu-timeout: cell exceeded 46 CPU s at seed-31; 32 of 32 seeds ran"),
            "chaos distinct=8 passes=31 failures=1 normalized_entropy=0.9180".into(),
        );
        assert_eq!(outcome, "ERROR");
        assert_eq!(
            reason.as_deref(),
            Some("seed-31 timed out before producing a comparison")
        );

        let mut outcome = "PASS".to_string();
        let mut reason = None;
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            None,
            "chaos distinct=6 passes=32 failures=0 normalized_entropy=0.7000".into(),
        );
        assert_eq!(outcome, "FAIL");
        assert_eq!(
            reason.as_deref(),
            Some("chaos distinct=6 passes=32 failures=0 normalized_entropy=0.7000")
        );
    }

    /// The shape of the fp-reduction chaos/ptrace red in the local validation
    /// of 84da7b816939 (2026-09-29): seed 30 exited 0 as the cell's cumulative
    /// 46 CPU s ran out, so it timed out, the loop stopped and seed 31 never
    /// ran. The cell reason said only `chaos distinct=7 passes=31 failures=0`,
    /// counting the timed-out seed as a pass and naming no timeout.
    #[test]
    fn a_chaos_timeout_leads_the_reason_and_is_not_counted_as_a_pass() {
        let attempt = |index: &str, status: i32, timed_out: bool| {
            let mut attempt = attempt_with_sabre_evidence("{}");
            attempt.index = index.into();
            attempt.status = Some(status);
            attempt.timed_out = timed_out;
            if timed_out {
                attempt.outcome = "FAIL".into();
                attempt.error_kind = Some("cpu-timeout".into());
                attempt.reason = Some("cell exceeded 46 CPU s".into());
            }
            attempt
        };
        let attempts = [
            attempt("seed-28", 0, false),
            attempt("seed-29", 1, false),
            attempt("seed-30", 0, true),
        ];
        assert_eq!(chaos_population_counts(&attempts), (1, 1, 1));
        // A killed seed (no exit status) is not a failure either.
        let mut killed = attempt("seed-30", 0, true);
        killed.status = None;
        assert_eq!(
            chaos_population_counts(&[attempt("seed-28", 1, false), killed]),
            (0, 1, 1)
        );

        let incomplete = chaos_incomplete_reason(&attempts, 32);
        assert_eq!(
            incomplete.as_deref(),
            Some("cpu-timeout: cell exceeded 46 CPU s at seed-30; 3 of 32 seeds ran")
        );
        assert_eq!(chaos_incomplete_reason(&attempts[..2], 32), None);

        let mut outcome = "FAIL".to_string();
        let mut reason = incomplete.clone();
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            incomplete.as_deref(),
            "chaos distinct=2 passes=1 failures=1 timed_out=1 normalized_entropy=1.0000".into(),
        );
        assert_eq!(outcome, "FAIL");
        assert_eq!(
            reason.as_deref(),
            Some(
                "cpu-timeout: cell exceeded 46 CPU s at seed-30; 3 of 32 seeds ran; \
                 chaos distinct=2 passes=1 failures=1 timed_out=1 normalized_entropy=1.0000"
            )
        );
    }

    /// A seed that failed for a product reason before the CPU budget ran out
    /// keeps its reason in the cell reason, after the timeout's, whether or
    /// not the population floors pass; an attempt-level ERROR keeps its own.
    #[test]
    fn a_chaos_timeout_keeps_an_earlier_seeds_failure_reason() {
        let mut attempts = [
            attempt_with_sabre_evidence("{}"),
            attempt_with_sabre_evidence("{}"),
            attempt_with_sabre_evidence("{}"),
        ];
        for (attempt, index) in attempts.iter_mut().zip(["seed-4", "seed-5", "seed-30"]) {
            attempt.index = index.into();
            attempt.status = Some(0);
            attempt.reason = None;
        }
        attempts[1].outcome = "FAIL".into();
        attempts[1].reason = Some("expected stdout mismatch".into());
        attempts[2].outcome = "FAIL".into();
        attempts[2].timed_out = true;
        attempts[2].error_kind = Some("cpu-timeout".into());
        attempts[2].reason = Some("cell exceeded 46 CPU s".into());
        let expected = "cpu-timeout: cell exceeded 46 CPU s at seed-30; 3 of 32 seeds ran; \
                        seed-5: expected stdout mismatch";
        let incomplete = chaos_incomplete_reason(&attempts, 32);
        assert_eq!(incomplete.as_deref(), Some(expected));

        // Every floor passes: the cell reason is the timeout, then seed 5's.
        let first_reason = attempts.iter().find_map(|attempt| attempt.reason.clone());
        assert_eq!(first_reason.as_deref(), Some("expected stdout mismatch"));
        assert_eq!(
            chaos_cell_reason("FAIL", first_reason.clone(), incomplete.as_deref()).as_deref(),
            Some(expected)
        );
        // An attempt-level ERROR keeps its reason; no timeout keeps the first.
        assert_eq!(
            chaos_cell_reason("ERROR", first_reason.clone(), incomplete.as_deref()),
            first_reason
        );
        assert_eq!(
            chaos_cell_reason("FAIL", first_reason.clone(), None),
            first_reason
        );

        // A floor fails too: the assertion follows both.
        let mut outcome = "FAIL".to_string();
        let mut reason = chaos_cell_reason("FAIL", first_reason, incomplete.as_deref());
        apply_failed_chaos_assertion(
            &mut outcome,
            &mut reason,
            incomplete.as_deref(),
            "chaos distinct=1 passes=1 failures=1 timed_out=1 normalized_entropy=0.0000".into(),
        );
        assert_eq!(
            reason.as_deref(),
            Some(
                "cpu-timeout: cell exceeded 46 CPU s at seed-30; 3 of 32 seeds ran; \
                 seed-5: expected stdout mismatch; \
                 chaos distinct=1 passes=1 failures=1 timed_out=1 normalized_entropy=0.0000"
            )
        );
    }

    fn no_result_with_exit_status(
        status: i32,
        reason: crate::canonical_verdict::NoResultReason,
    ) -> AttemptResult {
        static NEXT_DIRECTORY: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let dir = loop {
            let sequence = NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "hermit-runner-no-result-bracket-{}-{status}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("creating no-result fixture directory: {error}"),
            }
        };
        let verdict = dir.join("verdict.json");
        let mut report = VerificationReport::no_result();
        report.no_result_reason = Some(reason);
        let report = serde_json::to_string(&report).unwrap();
        let spec = CellRunSpec {
            id: CellId {
                test: "fixture/no-result".into(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            lane: "portable".into(),
            category: "fixture".into(),
            cwd: dir.clone(),
            env: BTreeMap::new(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"printf %s "$1" > "$2"; exit "$3""#.into(),
                "sh".into(),
                report,
                verdict.to_string_lossy().into_owned(),
                status.to_string(),
            ],
            guest_argv: vec!["fixture".into()],
            timeout_seconds: 5,
            verdict_path: Some(verdict),
            verification_log_dir: None,
            sabre_path_evidence: None,
            cell_dir: dir.clone(),
            expected_guest_exit: None,
            expected_stdout: None,
            expected_stdout_contains: None,
            comparator: Comparator::Strict,
            attempt: "1".into(),
            fixed_workdir_source: dir.join("workdir/1"),
            normalize_ptrace_golden: false,
        };
        let result = execute_spec(&spec).unwrap();
        fs::remove_dir_all(dir).unwrap();
        result
    }

    #[test]
    fn container_failure_is_a_product_crash_without_comparison_credit() {
        use crate::canonical_verdict::ContainerDisposition;
        use crate::canonical_verdict::ContainerFailure;
        use crate::canonical_verdict::NoResultReason;
        use crate::canonical_verdict::VerificationRun;
        for run in [VerificationRun::Run1, VerificationRun::Run2] {
            for disposition in [
                ContainerDisposition::Exited { code: 0 },
                ContainerDisposition::Signaled {
                    signal: 14,
                    core_dumped: false,
                },
            ] {
                let reason = NoResultReason::ContainerFailed(ContainerFailure { run, disposition });
                let failed = no_result_with_exit_status(125, reason.clone());
                assert_eq!(failed.outcome, "FAIL");
                assert_eq!(failed.error_kind, None);
                assert_eq!(failed.status, Some(125));
                assert_eq!(
                    observed_result(
                        "verify",
                        &failed.outcome,
                        std::slice::from_ref(&failed),
                        failed.error_kind.as_deref()
                    ),
                    Some(ObservedResult::CrashError)
                );
                assert_eq!(
                    failure_class(
                        &failed.outcome,
                        Some(ObservedResult::CrashError),
                        failed.error_kind.as_deref()
                    ),
                    Some(FailureClass::ProductFailure)
                );
                let report = VerificationReport::from_current_json_slice(
                    failed.verification_report.as_ref().unwrap().as_bytes(),
                )
                .unwrap();
                assert!(report.require_canonical_comparison().is_err());
                assert!(report.guest_exit_code.is_none() && report.guest_signal.is_none());
                let contradicted = no_result_with_exit_status(0, reason);
                assert_eq!(contradicted.outcome, "ERROR");
                assert_eq!(
                    contradicted.error_kind.as_deref(),
                    Some("incomplete-verification-evidence")
                );
                assert!(
                    contradicted
                        .reason
                        .unwrap()
                        .contains("contradicts Hermit process")
                );
            }
        }
    }

    #[test]
    fn no_result_preserves_the_process_outcome_distinction() {
        let failed = no_result_with_exit_status(
            7,
            crate::canonical_verdict::NoResultReason::FirstRunRejected {
                exit_code: Some(7),
                signal: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
            },
        );
        assert_eq!(failed.outcome, "FAIL");
        assert_eq!(
            observed_result(
                "verify",
                &failed.outcome,
                std::slice::from_ref(&failed),
                failed.error_kind.as_deref()
            ),
            Some(ObservedResult::CrashError)
        );
        assert_eq!(
            failure_class(
                &failed.outcome,
                Some(ObservedResult::CrashError),
                failed.error_kind.as_deref()
            ),
            Some(FailureClass::ProductFailure)
        );
        assert_eq!(failed.error_kind, None);
        assert_eq!(failed.status, Some(7));
        assert!(
            failed
                .reason
                .as_deref()
                .unwrap()
                .contains("before producing a terminal comparison")
        );

        let not_run =
            no_result_with_exit_status(127, crate::canonical_verdict::NoResultReason::NotRun);
        assert_eq!(not_run.outcome, "ERROR");
        assert_eq!(
            failure_class(&not_run.outcome, None, not_run.error_kind.as_deref()),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            not_run.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(not_run.status, Some(127));

        let unknown =
            no_result_with_exit_status(0, crate::canonical_verdict::NoResultReason::NotRun);
        assert_eq!(unknown.outcome, "ERROR");
        assert_eq!(
            failure_class(&unknown.outcome, None, unknown.error_kind.as_deref()),
            Some(FailureClass::NoResult)
        );
        assert_eq!(
            unknown.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(unknown.status, Some(0));
    }

    #[test]
    fn failed_preparation_surfaces_its_own_diagnostic() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-preparation-diagnostic-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(cell_dir.join("captures")).unwrap();
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let error = run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &[
                "-c".into(),
                "printf 'error: failed to write fixture: Permission denied\\n' >&2; exit 17".into(),
            ],
            Instant::now() + Duration::from_secs(5),
            5,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("fixture preparation failed for /bin/sh: exited 17"));
        assert!(error.contains("error: failed to write fixture: Permission denied"));
        assert!(!error.contains("no output was captured"));

        fs::remove_file(cell_dir.join("captures/prepare.stderr")).unwrap();
        let empty = with_diagnostic(
            "fixture preparation failed".into(),
            &cell_dir.join("captures"),
        );
        assert!(empty.contains("no output was captured"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_uses_the_named_cells_wall_deadline() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-preparation-timeout-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(cell_dir.join("captures")).unwrap();
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root: root.join("build"),
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: false,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let test = recipe(true);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 1,
            cpu_timeout_seconds: 1,
        };

        let error = run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &["-c".into(), "sleep 60".into()],
            Instant::now() + Duration::from_secs(1),
            cell.timeout_seconds,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            error.contains("cell exceeded 1 s during fixture preparation"),
            "{error}"
        );
        let result = infrastructure_error_result(&context, &cell, error);
        assert_eq!(result.test, "fixture/test");
        assert_eq!(result.error_kind.as_deref(), Some("infrastructure"));
        assert!(
            result
                .reason
                .as_deref()
                .unwrap()
                .contains("cell exceeded 1 s during fixture preparation")
        );

        run_preparation(
            &context,
            &cell_dir,
            "/bin/sh",
            &["-c".into(), "true".into()],
            Instant::now() + Duration::from_secs(1),
            cell.timeout_seconds,
            &mut Vec::new(),
        )
        .expect("healthy fixture preparation must finish silently under the same bound");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_does_not_consume_the_guest_execution_budget() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-separate-preparation-budget-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let program = root.join("fixture.sh");
        fs::write(
            &program,
            "#!/bin/sh\ncase \"$1\" in\n  --prepare) sleep 1.1 ;;\n  --run) sleep 1.1; printf 'complete\\n' ;;\n  *) exit 64 ;;\nesac\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&program, permissions).unwrap();

        let mut test = recipe(true);
        test.id = "fixture/separate-preparation-budget".into();
        test.program = Some("fixture.sh".into());
        test.direct = None;
        let mut mode = test.modes.remove("verify").unwrap();
        mode.runs = Some(1);
        mode.assert = Some(Assertions {
            min_distinct: Some(1),
            ..Assertions::default()
        });
        test.modes.insert("naked".into(), mode);
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "naked".into(),
                backend: None,
            },
            test,
            enabled: true,
            timeout_seconds: 2,
            cpu_timeout_seconds: 1,
        };
        let context = run_context(&root);

        let result = run_cell(&context, &cell).unwrap();
        assert_eq!(result.outcome, "PASS", "unexpected result: {result:?}");
        assert_eq!(result.result, Some(ObservedResult::Pass));
        assert_eq!(result.attempts.len(), 1);
        let observations = &result.cpu_observations.as_ref().unwrap().invocations;
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].role, InvocationRole::Preparation);
        assert_eq!(observations[0].live, LiveCpuObservation::Disabled);
        assert_eq!(observations[1].ordinal, 2);
        assert_eq!(
            result.cpu_usage_usec,
            Some(measured_final_cpu(&observations[0]) + measured_final_cpu(&observations[1]))
        );
        assert_cpu_source_roundtrip(&result);
        assert!(!result.attempts[0].timed_out);
        assert_eq!(result.attempts[0].stdout, "complete\n");
        assert!(
            result.duration_ms.is_some_and(|duration| duration >= 2_000),
            "the fixture did not exercise separate preparation and execution time: {result:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prebuilt_copy_cannot_inherit_or_accept_a_nonexecutable_guest() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-prebuilt-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let build_root = root.join("build");
        let source = build_root.join("fixture-test/fixtures");
        let cell_dir = root.join("cell");
        fs::create_dir_all(&source).unwrap();

        let mut test = recipe(true);
        test.program = Some("tests/fixture.c".into());
        test.direct = None;
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 15,
            cpu_timeout_seconds: 10,
        };
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: root.join("results"),
            build_root,
            run_id: "fixture".into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: fixture_host_capabilities(),
            attempt: 1,
            run_index: None,
            epoch: "2026-01-01T00:00:00Z".into(),
            source_sha: "0".repeat(40),
            binary_build_sha: None,
            source_dirty: false,
            prebuilt: true,
            keep_logs: false,
            parity_retained: BTreeSet::new(),
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };

        fs::create_dir_all(cell_dir.join("fixtures")).unwrap();
        fs::write(cell_dir.join("fixtures/program"), b"stale").unwrap();
        fs::set_permissions(
            cell_dir.join("fixtures/program"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_err());
        assert!(!cell_dir.join("fixtures/program").exists());

        fs::write(source.join("program"), b"new").unwrap();
        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_err());

        fs::set_permissions(source.join("program"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(prepare_test(&context, &cell, &cell_dir).is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    fn build_info_json(git_sha: &str) -> Vec<u8> {
        serde_json::to_vec(&detcore_model::build_info::BuildInfo {
            schema: detcore_model::build_info::BuildInfo::SCHEMA,
            version: "0.2.0".into(),
            build_date: Some("2026-08-25".into()),
            git_sha: git_sha.into(),
            features: detcore_model::build_info::BuildFeatures {
                dbt: false,
                e9patch: false,
                sabre: false,
            },
        })
        .unwrap()
    }

    /// The binary's own revision is read from its typed record, including the
    /// dirty marker, which the checkout HEAD cannot supply because it describes
    /// a different tree at a different time.
    #[test]
    fn binary_build_sha_is_read_from_the_typed_build_record() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e-dirty")).as_deref(),
            Some("351cd3603f7e-dirty"),
            "a dirty build must keep saying so; that is the fact the checkout cannot supply"
        );
        assert_eq!(
            parse_binary_build_info(&build_info_json("3d85028b3bca")).as_deref(),
            Some("3d85028b3bca")
        );
        // 40-hex is equally acceptable; the width is not the contract.
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e537297067e07a20c5ccf7a23c0e0"))
                .as_deref(),
            Some("351cd3603f7e537297067e07a20c5ccf7a23c0e0")
        );
    }

    /// "I do not know" is a value the binary can state, and it must survive as
    /// one rather than collapsing into "I could not ask".
    #[test]
    fn an_unknown_revision_is_preserved_not_dropped() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("unknown")).as_deref(),
            Some("unknown")
        );
    }

    /// Nothing recognisable must yield `None`, never a guess. A provenance that
    /// could not be established must not be reported as one that matched.
    #[test]
    fn malformed_or_unsupported_build_records_establish_nothing() {
        for bytes in [
            b"".as_slice(),
            br#"{"schema":1}"#,
            br#"{"schema":2,"version":"0.2.0","build_date":null,"git_sha":"351cd3603f7e","features":{"dbt":false,"e9patch":false,"sabre":false}}"#,
            &build_info_json("zzzz"),
        ] {
            assert_eq!(
                parse_binary_build_info(bytes),
                None,
                "must not invent provenance from {}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    /// Mutating the producer's typed value must move the consumer's value.
    #[test]
    fn binary_build_sha_follows_the_typed_field() {
        assert_eq!(
            parse_binary_build_info(&build_info_json("351cd3603f7e")),
            Some("351cd3603f7e".into())
        );
        assert_eq!(
            parse_binary_build_info(&build_info_json("aaaaaaaaaaaa")),
            Some("aaaaaaaaaaaa".into())
        );
    }

    /// The two fields answer different questions and must be independently
    /// settable, because in practice they disagree: the checkout moves during a
    /// rebase-and-rerun loop while the binary on disk does not.
    #[test]
    fn checkout_sha_and_binary_provenance_are_separate_facts() {
        let checkout = "affda5d9840baeb60c5f5aa9c7b0ff5560e81ef3";
        let built_from = parse_binary_build_info(&build_info_json("351cd3603f7e-dirty"))
            .expect("typed build record parses");
        assert_ne!(
            checkout,
            built_from.trim_end_matches("-dirty"),
            "the case this field exists for: the harness stood at one commit and ran a \
             binary built from another"
        );
        assert!(
            !checkout.contains("-dirty") && built_from.ends_with("-dirty"),
            "and the checkout cannot express the build tree's dirtiness at all"
        );
    }

    /// REGRESSION CHECK FOR THE ORDINARY-HOST FALLBACK.
    ///
    /// Without the default the guest inherits the HOST's cwd verbatim. Measured on
    /// 2026-08-26 at main f4de43461a, 200 runs rotating across four checkouts:
    ///   default off -> 4 distinct guest `pwd` values
    ///   default on  -> 1, `/tmp/test`, and the directory empty on all 200
    /// The explicit hermetic path below supersedes this with a tmpfs at `/test`.
    /// This fallback remains for direct non-hermetic harness runs.
    #[test]
    fn fixed_workdir_is_bound_by_default_and_withheld_where_it_would_lie() {
        let source = Path::new("/cells/x/workdir/attempt-7");
        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, "ptrace", None, None, Some(source));
        assert_eq!(
            argv,
            vec![
                "--bind=/cells/x/workdir/attempt-7:/tmp/test".to_string(),
                "--workdir".to_string(),
                "/tmp/test".to_string(),
            ],
            "the fallback must bind a fresh per-attempt directory AND chdir into it"
        );

        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, "ptrace", None, Some("/tmp"), Some(source));
        assert_eq!(argv, vec!["--workdir".to_string(), "/tmp".to_string()]);

        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(&mut argv, "ptrace", None, None, None);
        assert!(
            argv.is_empty(),
            "a cell that cannot honour a workdir must be given none, got {argv:?}"
        );
    }

    #[test]
    fn the_hermetic_lane_outranks_manifest_and_fallback_workdirs() {
        let mut argv: Vec<String> = Vec::new();
        append_execution_root_args(
            &mut argv,
            "ptrace",
            Some(Path::new(HERMETIC_TEST_WORKDIR)),
            Some("/tmp"),
            Some(Path::new("/cells/x/workdir/attempt-7")),
        );
        assert_eq!(
            argv,
            vec![
                format!("--mount=type=tmpfs,target={HERMETIC_TEST_WORKDIR}"),
                "--workdir".to_string(),
                HERMETIC_TEST_WORKDIR.to_string(),
            ],
            "the explicit hermetic request must suppress both the manifest override and fallback"
        );
        assert!(
            !argv.iter().any(|arg| arg.starts_with("--bind")),
            "the ordinary-host bind leaked into the /test path: {argv:?}"
        );

        let mut dbt_argv = Vec::new();
        append_execution_root_args(
            &mut dbt_argv,
            "dbt",
            Some(Path::new(HERMETIC_TEST_WORKDIR)),
            None,
            None,
        );
        assert_eq!(
            dbt_argv,
            vec!["--workdir".to_string(), HERMETIC_TEST_WORKDIR.to_string()],
            "DBT's marked physical adapter supplies /test without accepting the generic mount option"
        );
    }

    #[test]
    fn test_workdir_support_matches_the_modes_that_can_honour_it() {
        for mode in ["verify", "replay", "chaos", "custom"] {
            assert!(supports_test_workdir(mode, "ptrace"), "mode={mode}");
            assert!(supports_test_workdir(mode, "dbt"), "mode={mode}");
        }
        assert!(!supports_test_workdir("naked", "native"));
    }

    #[test]
    fn selected_supported_cells_do_not_override_the_hermetic_test_workdir() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let manifests = ManifestSet::load(&root).unwrap();
        let mut overrides = Vec::new();
        for document in &manifests.documents {
            for test in &document.test {
                for (mode, recipe) in &test.modes {
                    let selection = ci_selection(recipe).unwrap();
                    for backend in &recipe.backends_enabled {
                        if selection.selected(backend)
                            && supports_test_workdir(mode, backend)
                            && recipe.workdir.is_some()
                        {
                            overrides.push(format!("{}/{mode}/{backend}", test.id));
                        }
                    }
                }
            }
        }
        assert!(
            overrides.is_empty(),
            "scheduled supported cells must all use the hermetic /test workdir: {overrides:?}"
        );
    }

    #[test]
    fn every_selected_cell_can_honour_the_hermetic_test_workdir() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let expected: JsonValue =
            serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap())
                .unwrap();
        let unsupported = expected["cells"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|cell| {
                let mode = cell["mode"].as_str().unwrap();
                let backend = cell["backend"].as_str().unwrap();
                (!supports_test_workdir(mode, backend))
                    .then(|| format!("{}/{mode}/{backend}", cell["test"].as_str().unwrap()))
            })
            .collect::<Vec<_>>();
        assert!(
            unsupported.is_empty(),
            "selected cells that cannot honour the hermetic /test gate: {unsupported:?}"
        );
    }

    #[test]
    fn attempt_labels_must_be_one_normal_path_component() {
        for attempt in [
            "",
            ".",
            "..",
            "/absolute",
            "../parent",
            "parent/child",
            "normal/",
        ] {
            assert_eq!(
                fixed_workdir_source_for_attempt(Path::new("/cell"), attempt),
                Err(format!(
                    "invalid attempt label {attempt:?}: expected exactly one normal path component"
                ))
            );
        }
        for attempt in ["1", "attempt-7", "seed--42"] {
            assert_eq!(
                fixed_workdir_source_for_attempt(Path::new("/cell"), attempt),
                Ok(PathBuf::from(format!("/cell/workdir/{attempt}")))
            );
        }
    }

    /// The label is rejected BEFORE anything is created on disk.
    ///
    /// `attempt_labels_must_be_one_normal_path_component` covers the return value
    /// against a path that is never touched, so it cannot see an ordering defect.
    /// If the guard ever moves below the first `create_dir_all`, a traversal label
    /// gets to create a directory outside the cell before being refused, and the
    /// return value alone still looks correct.
    #[test]
    fn an_invalid_attempt_label_is_refused_before_any_filesystem_side_effect() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-attempt-label-ordering-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell = ptrace_cell("verify");
        let context = run_context(&root);
        let cell_dir = root.join("cell");

        for attempt in ["..", "/absolute", "parent/child"] {
            let error = build_spec(
                &context,
                &cell,
                cell_dir.clone(),
                vec!["/bin/true".into()],
                attempt,
                None,
                cell.timeout_seconds,
            )
            .unwrap_err();
            assert_eq!(
                error,
                format!(
                    "invalid attempt label {attempt:?}: expected exactly one normal path component"
                )
            );
            assert!(
                !cell_dir.exists(),
                "an invalid attempt label must be rejected before filesystem operations"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fixed_workdir_bind_is_distinct_for_concurrent_attempts() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-concurrent-workdir-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let cell_dir = root.join("cell");
        fs::create_dir_all(&cell_dir).unwrap();
        let cell = ptrace_cell("verify");
        let context = run_context(&root);
        let source_for = |attempt: &str| {
            let spec = build_spec(
                &context,
                &cell,
                cell_dir.clone(),
                vec!["/bin/true".into()],
                attempt,
                None,
                cell.timeout_seconds,
            )
            .unwrap();
            bound_workdir_source(&spec)
        };
        let sources = [source_for("1"), source_for("2")];
        assert_eq!(sources[0], cell_dir.join("workdir/1"));
        assert_eq!(sources[1], cell_dir.join("workdir/2"));
        assert_ne!(sources[0], sources[1]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execute_spec_resets_the_exact_bound_workdir() {
        let root = std::env::temp_dir().join(format!(
            "hermit-runner-workdir-reset-bracket-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cell_dir = root.join("cell");
        let cell = ptrace_cell("custom");
        let context = run_context(&root);
        let mut spec = build_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            "attempt-7",
            None,
            cell.timeout_seconds,
        )
        .unwrap();
        let bound = bound_workdir_source(&spec);
        assert_eq!(bound, cell_dir.join("workdir/attempt-7"));
        assert_eq!(bound, spec.fixed_workdir_source);

        fs::create_dir_all(&bound).unwrap();
        fs::write(bound.join("stale"), b"left by an earlier run").unwrap();
        spec.argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"test -d "$1" && test ! -e "$1/stale" && : > "$1/myfile.txt""#.into(),
            "sh".into(),
            bound.to_string_lossy().into_owned(),
        ];

        let result = execute_spec(&spec).unwrap();
        assert_eq!(result.index, "attempt-7");
        assert_eq!(result.outcome, "PASS", "{}", result.stderr);
        assert!(bound.join("myfile.txt").is_file());
        assert!(!bound.join("stale").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
