//! Establish and verify the outer safe-ci scope used by in-process DAG clients.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::cgroup::CgroupManager;
use dagrun::cgroup::Cgroups;
use dagrun::cgroup::ContainmentProof;
use dagrun::cgroup::attempt_scope_reexec;
use dagrun::cgroup::expected_outer_memory_max_bytes;
use dagrun::cgroup::expected_scope_runtime_max_s;
use dagrun::cgroup::install_scope_teardown;
use dagrun::cgroup::is_in_scope;
use dagrun::cgroup::verify_scope_runtime_max;
use dagrun::scheduler::BoxedCgroups;

/// True while a check that is EXPECTED to refuse is running.
///
/// The self-test drives every refusal path on purpose, so those paths emit
/// diagnostics on a healthy run. Routing them through a distinct marker keeps
/// `[safe-ci] ERROR:` meaning "something is actually wrong": a grep, a log
/// scraper, or a person skimming a green run must not be told it failed.
static EXPECTED_REFUSAL: AtomicBool = AtomicBool::new(false);

/// The marker for diagnostics emitted while a refusal is being verified.
fn diag_marker() -> &'static str {
    if EXPECTED_REFUSAL.load(Ordering::Relaxed) {
        "[safe-ci] expected-refusal:"
    } else {
        "[safe-ci] ERROR:"
    }
}

/// Run a check whose refusal is the assertion, not a fault.
///
/// Scoped rather than set for the whole self-test: a POSITIVE control inside
/// the same self-test must still report a genuine fault as ERROR.
fn expecting_refusal<T>(check: impl FnOnce() -> T) -> T {
    EXPECTED_REFUSAL.store(true, Ordering::Relaxed);
    let observed = check();
    EXPECTED_REFUSAL.store(false, Ordering::Relaxed);
    observed
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScopeRequirement {
    LiveContainment,
    OuterMemorySwapAndOomGroup,
    RuntimeMax,
    PerStepCgroups,
}

fn requirement_message(requirement: ScopeRequirement) -> &'static str {
    match requirement {
        ScopeRequirement::LiveContainment => {
            "the in-scope claim is not supported by the live process"
        }
        ScopeRequirement::OuterMemorySwapAndOomGroup => {
            "outer MemoryMax/MemorySwapMax/memory.oom.group readback failed"
        }
        ScopeRequirement::RuntimeMax => "this invocation's outer RuntimeMaxSec readback failed",
        ScopeRequirement::PerStepCgroups => {
            "the observed outer scope could not create per-step cgroups"
        }
    }
}

fn require_observed(requirement: ScopeRequirement, observed: bool) -> Result<(), &'static str> {
    if observed {
        Ok(())
    } else {
        Err(requirement_message(requirement))
    }
}

fn runtime_readback_satisfies(requirement_is_active: bool, observed: bool) -> bool {
    !requirement_is_active || observed
}

#[derive(Clone, Copy, Debug)]
struct ScopeObservations {
    live_containment: bool,
    outer_memory_swap_and_oom_group: bool,
    runtime_max: bool,
    per_step_cgroups: bool,
}

/// The same ordered refusal decision used by the live path and the inert
/// bracket. Keeping the observations as typed inputs makes bypassing any one of
/// them observable without pretending a unit test can manufacture a live
/// systemd scope.
fn require_scope_observations(
    observations: ScopeObservations,
    verify_runtime: bool,
) -> Result<(), &'static str> {
    require_observed(
        ScopeRequirement::LiveContainment,
        observations.live_containment,
    )?;
    require_observed(
        ScopeRequirement::OuterMemorySwapAndOomGroup,
        observations.outer_memory_swap_and_oom_group,
    )?;
    require_observed(
        ScopeRequirement::RuntimeMax,
        runtime_readback_satisfies(verify_runtime, observations.runtime_max),
    )?;
    require_observed(
        ScopeRequirement::PerStepCgroups,
        observations.per_step_cgroups,
    )?;
    Ok(())
}

fn unavailable(label: &str, allow_failure: bool, message: &str) -> Result<BoxedCgroups, u8> {
    if allow_failure {
        eprintln!("{label}: WARNING: {message}; running UNBOXED (--allow-cgroup-failure).");
        Ok(None)
    } else {
        eprintln!("{label}: ERROR: {message}.");
        Err(3)
    }
}

/// Keep the helper's typed refusal load-bearing at both Rust call sites. The
/// self-test feeds this an Err(3), so replacing it with a discarded result and
/// an unboxed success is behaviorally detected.
pub fn propagate_result(result: Result<BoxedCgroups, u8>) -> Result<BoxedCgroups, u8> {
    result
}

/// Find the exact promised scope in the ancestry of the cgroup observed for
/// this live process. A scheduler child normally lives in `step-*`, one level
/// below that scope; treating only the current cgroup as the scope makes a
/// correctly nested validate falsely fail its outer-limit audit.
fn promised_scope_ancestor(proof: &ContainmentProof) -> Option<PathBuf> {
    let promised = proof.unit.as_deref()?;
    proof.cgroup.ancestors().find_map(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| *name == promised)
            .map(|_| ancestor.to_path_buf())
    })
}

/// Whether this invocation owns teardown of the promised outer scope.
///
/// The owner is observed at the exact scope root before `Cgroups::new()` moves
/// it. Any descendant—including a direct or nested `supervisor`—is inherited;
/// installing the outer SIGINT/SIGTERM handler there would let one node stop
/// the entire run and every sibling.
fn invocation_owns_promised_scope(proof: &ContainmentProof) -> bool {
    let Some(scope) = promised_scope_ancestor(proof) else {
        return false;
    };
    proof.cgroup == scope
}

fn read_trim(group: &Path, name: &str) -> Option<String> {
    fs::read_to_string(group.join(name))
        .ok()
        .map(|value| value.trim().to_string())
}

/// Write and read back the OOM-group bit, then verify every outer memory
/// control against the exact scope named by the live containment proof.
fn verify_outer_scope_limits_at(scope: &Path, expected_memory_max: i64) -> bool {
    let oom_control = scope.join("memory.oom.group");
    if let Err(error) = fs::write(&oom_control, "1") {
        eprintln!(
            "{} outer memory.oom.group=1 write failed at {} ({error})",
            diag_marker(),
            oom_control.display()
        );
        return false;
    }

    outer_scope_limit_readback_matches(scope, expected_memory_max)
}

fn outer_scope_limit_readback_matches(scope: &Path, expected_memory_max: i64) -> bool {
    let memory_max = read_trim(scope, "memory.max");
    let memory_swap_max = read_trim(scope, "memory.swap.max");
    let memory_oom_group = read_trim(scope, "memory.oom.group");
    let memory_ok = memory_max
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .is_some_and(|actual| actual <= expected_memory_max && expected_memory_max - actual < 4096);
    let swap_ok = memory_swap_max.as_deref() == Some("0");
    let oom_group_ok = memory_oom_group.as_deref() == Some("1");
    eprintln!(
        "[safe-ci]{} outer cgroup audit at {}: memory.max={} ({}), memory.swap.max={} ({}), \
         memory.oom.group={} ({})",
        if EXPECTED_REFUSAL.load(Ordering::Relaxed) {
            " expected-refusal:"
        } else {
            ""
        },
        scope.display(),
        memory_max.as_deref().unwrap_or("UNREADABLE"),
        if memory_ok { "bound" } else { "MISMATCH" },
        memory_swap_max.as_deref().unwrap_or("UNREADABLE"),
        if swap_ok { "disabled" } else { "MISMATCH" },
        memory_oom_group.as_deref().unwrap_or("UNREADABLE"),
        if oom_group_ok { "enabled" } else { "MISMATCH" },
    );
    memory_ok && swap_ok && oom_group_ok
}

fn outer_scope_limits_observed(proof: Option<&ContainmentProof>, expected_memory_max: i64) -> bool {
    let Some(proof) = proof else {
        eprintln!(
            "{} outer cgroup limit audit has no live containment proof",
            diag_marker()
        );
        return false;
    };
    let Some(scope) = promised_scope_ancestor(proof) else {
        eprintln!(
            "{} observed cgroup {} has no ancestor matching promised unit {}",
            diag_marker(),
            proof.cgroup.display(),
            proof.unit.as_deref().unwrap_or("<missing>")
        );
        return false;
    };
    verify_outer_scope_limits_at(&scope, expected_memory_max)
}

// --------------------------------------------------------------- CPU placement
//
// https://github.com/rrnewton/hermit/issues/3265: on the development host
// recorded in docs/TESTING_ENVIRONMENTS.md ("Named measurement hosts") a
// ptrace-stop-heavy cell costs 7.6-7.9x the CPU on the CPUs the AMD uncore and
// L3 PMUs are bound to. The dev-hermit launcher (ci-hub/validate/cpu_placement.py)
// derives the allowed set from the host and hands it down in the variables
// below. The launcher's own `AllowedCPUs=` stops at this process's re-exec into
// a dagrun scope in another slice, so the scope owner re-applies the same list
// to that scope. Every step, container and cell below inherits it.
//
// Placement is a performance measure, never a gate: a failure to apply is
// reported and recorded, and the run continues exactly as before. Nothing here
// changes a budget, a timeout, which cells run, or how a verdict is computed.

/// The CPU list the scope may use; empty means the launcher excluded nothing.
pub const CPU_PLACEMENT_ALLOWED_ENV: &str = "HERMIT_CI_ALLOWED_CPUS";
/// The CPU list the launcher excluded, for the record.
pub const CPU_PLACEMENT_EXCLUDED_ENV: &str = "HERMIT_CI_EXCLUDED_CPUS";
/// How the launcher chose (`sysfs-pmu-cpumask`, `override`, `sysfs-none`, ...).
pub const CPU_PLACEMENT_SOURCE_ENV: &str = "HERMIT_CI_CPU_PLACEMENT_SOURCE";

/// How long a scope owner waits for systemd to realize the cpuset.
const CPU_PLACEMENT_READBACK: Duration = Duration::from_secs(3);

/// The highest CPU number any Linux kernel can have: CONFIG_NR_CPUS tops out at
/// 8192 (x86 MAXSMP), numbered from 0. A list naming a higher CPU is malformed,
/// and is refused BEFORE its range is expanded, so a value such as
/// `0-4294967295` cannot make this process build a four-billion-element set.
const MAX_CPU_NUMBER: u32 = 8191;

/// Parse a kernel CPU list (`0-3,8`). `Some(empty)` for `""`, `None` if malformed
/// or if it names a CPU above [`MAX_CPU_NUMBER`].
fn parse_cpu_list(text: &str) -> Option<BTreeSet<u32>> {
    let text = text.trim();
    let mut cpus = BTreeSet::new();
    if text.is_empty() {
        return Some(cpus);
    }
    for part in text.split(',') {
        let (low, high) = match part.split_once('-') {
            Some((low, high)) => (low, high),
            None => (part, part),
        };
        let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
        if !digits(low) || !digits(high) {
            return None;
        }
        let low: u32 = low.parse().ok()?;
        let high: u32 = high.parse().ok()?;
        if high < low || high > MAX_CPU_NUMBER {
            return None;
        }
        cpus.extend(low..=high);
    }
    Some(cpus)
}

/// Why an observed CPU list does not honour the requested allowed set.
fn cpu_list_within(
    name: &str,
    observed: Option<&str>,
    requested: &BTreeSet<u32>,
) -> Result<(), String> {
    let Some(text) = observed else {
        return Err(format!("{name} is unreadable"));
    };
    let Some(cpus) = parse_cpu_list(text) else {
        return Err(format!("{name}={text:?} does not parse as a CPU list"));
    };
    if cpus.is_empty() {
        return Err(format!("{name} is empty"));
    }
    let outside: Vec<u32> = cpus.difference(requested).copied().collect();
    if !outside.is_empty() {
        return Err(format!(
            "{name}={text} still includes CPU(s) outside the allowed set: {outside:?}"
        ));
    }
    Ok(())
}

/// APPLIED iff both the scope's effective cpuset and this process's affinity
/// are non-empty subsets of the requested allowed set.
fn cpu_placement_verdict(
    requested: &BTreeSet<u32>,
    scope_effective: Option<&str>,
    affinity: Option<&str>,
) -> Result<(), String> {
    cpu_list_within("cpuset.cpus.effective", scope_effective, requested)?;
    cpu_list_within("Cpus_allowed_list", affinity, requested)
}

fn process_cpus_allowed_list() -> Option<String> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
        .map(|value| value.trim().to_string())
}

/// Format a CPU set as a kernel CPU list (`1-3,8`).
fn format_cpu_list(cpus: &BTreeSet<u32>) -> String {
    let mut parts = Vec::new();
    let mut iter = cpus.iter().copied().peekable();
    while let Some(low) = iter.next() {
        let mut high = low;
        while iter.peek() == Some(&(high + 1)) {
            high = iter.next().unwrap_or(high);
        }
        parts.push(if low == high {
            low.to_string()
        } else {
            format!("{low}-{high}")
        });
    }
    parts.join(",")
}

/// A placement the nested self-test can REQUIRE: this process's CPUs minus the
/// lowest one, as `(allowed, excluded)`, but only after a throwaway user unit
/// shows the user manager enforcing `AllowedCPUs=` on this host. `Err` names
/// why the placement path cannot be exercised here, so the caller can say so
/// instead of passing without testing it.
#[allow(dead_code)] // Only validate.rs's nested scope self-test uses it.
pub fn self_test_placement_request() -> Result<(String, String), String> {
    let current =
        process_cpus_allowed_list().ok_or("this process's Cpus_allowed_list is unreadable")?;
    let mut cpus = parse_cpu_list(&current)
        .ok_or_else(|| format!("Cpus_allowed_list={current:?} does not parse"))?;
    let Some(excluded) = cpus.pop_first() else {
        return Err("this process has no CPUs".into());
    };
    if cpus.is_empty() {
        return Err(format!(
            "this process may use only CPU {excluded}; nothing would remain"
        ));
    }
    let allowed = format_cpu_list(&cpus);
    let output = Command::new("timeout")
        .args([
            "20",
            "systemd-run",
            "--user",
            "--quiet",
            "--wait",
            "--pipe",
            "--collect",
        ])
        .arg(format!("--property=AllowedCPUs={allowed}"))
        .args(["--", "grep", "Cpus_allowed_list", "/proc/self/status"])
        .output()
        .map_err(|error| format!("systemd-run could not be run: {error}"))?;
    let observed = String::from_utf8_lossy(&output.stdout);
    let observed = observed
        .trim()
        .strip_prefix("Cpus_allowed_list:")
        .map(str::trim);
    if !output.status.success() {
        return Err(format!(
            "a probe unit with AllowedCPUs={allowed} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    cpu_list_within("the probe unit's Cpus_allowed_list", observed, &cpus).map_err(|reason| {
        format!("the user manager does not enforce AllowedCPUs= here: {reason}")
    })?;
    Ok((allowed, excluded.to_string()))
}

/// What this invocation did about CPU placement, for the log and the ledger row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuPlacementObservation {
    /// `applied`, `not-applied`, `inherited`, `unrestricted`, or `not-requested`.
    pub status: &'static str,
    pub source: Option<String>,
    pub excluded_cpus: Option<String>,
    pub requested_allowed_cpus: Option<String>,
    pub scope_unit: Option<String>,
    pub scope_cpuset_effective: Option<String>,
    pub cpus_allowed_list: Option<String>,
    pub detail: String,
}

impl CpuPlacementObservation {
    /// The `cpu_placement` ledger-row extension.
    #[allow(dead_code)] // pressure-test.rs includes this module but writes no ledger row.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status,
            "source": self.source,
            "excluded_cpus": self.excluded_cpus,
            "requested_allowed_cpus": self.requested_allowed_cpus,
            "scope_unit": self.scope_unit,
            "scope_cpuset_effective": self.scope_cpuset_effective,
            "cpus_allowed_list": self.cpus_allowed_list,
            "detail": self.detail,
        })
    }
}

/// The completed-run summary line naming the placement status and the excluded
/// CPUs (https://github.com/rrnewton/hermit/issues/3265). Informational only: the
/// caller computes the verdict from the exit code alone, before this line exists.
#[allow(dead_code)] // pressure-test.rs includes this module but prints no run summary.
pub fn cpu_placement_summary_line(observation: Option<&CpuPlacementObservation>) -> String {
    let Some(observation) = observation else {
        return "CPU placement: not observed (this invocation never resolved its cgroups, so \
                no placement was applied or inherited); informational, not part of the verdict"
            .into();
    };
    let excluded = match observation.excluded_cpus.as_deref().map(str::trim) {
        None => "not set",
        Some("") => "none",
        Some(cpus) => cpus,
    };
    format!(
        "CPU placement: {} (excluded CPUs: {excluded}; source: {}; Cpus_allowed_list: {}): {}; \
         informational, not part of the verdict",
        observation.status,
        observation.source.as_deref().unwrap_or("not set"),
        observation
            .cpus_allowed_list
            .as_deref()
            .unwrap_or("UNREADABLE"),
        observation.detail,
    )
}

static CPU_PLACEMENT: OnceLock<CpuPlacementObservation> = OnceLock::new();

/// This process's placement observation, once `resolve_cgroups` has made one.
#[allow(dead_code)] // pressure-test.rs includes this module but writes no ledger row.
pub fn cpu_placement_observation() -> Option<&'static CpuPlacementObservation> {
    CPU_PLACEMENT.get()
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Apply (scope owner) or observe (inherited) the launcher's CPU placement.
/// Never refuses: every failure becomes a `not-applied` observation.
fn establish_cpu_placement(
    label: &str,
    proof: Option<&ContainmentProof>,
    owns_outer_scope: bool,
) -> CpuPlacementObservation {
    let source = nonempty_env(CPU_PLACEMENT_SOURCE_ENV);
    let excluded_cpus = std::env::var(CPU_PLACEMENT_EXCLUDED_ENV).ok();
    let requested_text = std::env::var(CPU_PLACEMENT_ALLOWED_ENV).ok();
    let scope = proof.and_then(promised_scope_ancestor);
    let scope_unit = proof.and_then(|proof| proof.unit.clone());
    let read_scope_effective = || {
        scope
            .as_deref()
            .and_then(|scope| read_trim(scope, "cpuset.cpus.effective"))
    };
    let observation = |status, detail: String| CpuPlacementObservation {
        status,
        source: source.clone(),
        excluded_cpus: excluded_cpus.clone(),
        requested_allowed_cpus: requested_text.clone(),
        scope_unit: scope_unit.clone(),
        scope_cpuset_effective: read_scope_effective(),
        cpus_allowed_list: process_cpus_allowed_list(),
        detail,
    };
    let Some(requested_text) = requested_text.as_deref() else {
        return observation(
            "not-requested",
            format!(
                "{CPU_PLACEMENT_ALLOWED_ENV} is not set; this run was not launched with a CPU placement"
            ),
        );
    };
    let Some(requested) = parse_cpu_list(requested_text) else {
        let result = observation(
            "not-applied",
            format!("{CPU_PLACEMENT_ALLOWED_ENV}={requested_text:?} does not parse as a CPU list"),
        );
        eprintln!(
            "{label}: WARNING: CPU placement NOT APPLIED: {}.",
            result.detail
        );
        return result;
    };
    if requested.is_empty() {
        return observation(
            "unrestricted",
            "the launcher excluded no CPU; no cpuset applied".into(),
        );
    }
    if !owns_outer_scope {
        let within = cpu_placement_verdict(
            &requested,
            read_scope_effective().as_deref(),
            process_cpus_allowed_list().as_deref(),
        );
        let detail = match within {
            Ok(()) => "inherited from the owning invocation; this process's CPUs are within the allowed set".to_string(),
            Err(reason) => format!("inherited from the owning invocation; NOT within the allowed set: {reason}"),
        };
        let result = observation("inherited", detail);
        eprintln!(
            "{label}: CPU placement inherited: Cpus_allowed_list={} ({}).",
            result.cpus_allowed_list.as_deref().unwrap_or("UNREADABLE"),
            result.detail
        );
        return result;
    }
    let (Some(scope_path), Some(unit)) = (scope.as_deref(), scope_unit.as_deref()) else {
        let result = observation(
            "not-applied",
            "no promised scope unit to apply AllowedCPUs to".into(),
        );
        eprintln!(
            "{label}: WARNING: CPU placement NOT APPLIED: {}.",
            result.detail
        );
        return result;
    };
    let property = format!("AllowedCPUs={}", requested_text.trim());
    // systemd owns the value (a raw cpuset.cpus write could be reverted by the
    // manager), and --runtime keeps it off disk for a transient unit.
    let applied = Command::new("systemctl")
        .args(["--user", "set-property", "--runtime", unit, &property])
        .output();
    let failure = match applied {
        Err(error) => Some(format!("systemctl could not be run: {error}")),
        Ok(output) if !output.status.success() => Some(format!(
            "`systemctl --user set-property --runtime {unit} {property}` exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(_) => None,
    };
    let result = if let Some(failure) = failure {
        observation("not-applied", failure)
    } else {
        // systemd realizes cgroup properties from its event loop, so the
        // readback may briefly lag the D-Bus reply.
        let deadline = Instant::now() + CPU_PLACEMENT_READBACK;
        loop {
            let effective = read_trim(scope_path, "cpuset.cpus.effective");
            let affinity = process_cpus_allowed_list();
            match cpu_placement_verdict(&requested, effective.as_deref(), affinity.as_deref()) {
                Ok(()) => {
                    break observation(
                        "applied",
                        format!(
                            "{property} set on {unit}; https://github.com/rrnewton/hermit/issues/3265"
                        ),
                    );
                }
                Err(reason) if Instant::now() >= deadline => {
                    break observation(
                        "not-applied",
                        format!("{property} was accepted by systemd but not observed: {reason}"),
                    );
                }
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    };
    if result.status == "applied" {
        eprintln!(
            "{label}: CPU placement APPLIED: {property} on {unit} (source={}, excluded={}); \
             scope cpuset.cpus.effective={}, Cpus_allowed_list={}.",
            result.source.as_deref().unwrap_or("unknown"),
            result.excluded_cpus.as_deref().unwrap_or("unknown"),
            result
                .scope_cpuset_effective
                .as_deref()
                .unwrap_or("UNREADABLE"),
            result.cpus_allowed_list.as_deref().unwrap_or("UNREADABLE"),
        );
    } else {
        eprintln!(
            "{label}: WARNING: CPU placement NOT APPLIED: {}; cells may run on the excluded CPUs {} \
             (https://github.com/rrnewton/hermit/issues/3265). The run continues unchanged.",
            result.detail,
            result.excluded_cpus.as_deref().unwrap_or("unknown"),
        );
    }
    result
}

/// Establish two-level cgroup-v2 boxing for a direct Rust scheduler client.
///
/// A successful initial call re-executes the current CLI inside a transient
/// scope. The in-scope call verifies the running process and every requested
/// outer limit before returning a per-step cgroup manager. `verify_runtime`
/// is false only when the caller inherited somebody else's scope rather than
/// requesting its own `RuntimeMaxSec` rung.
pub fn resolve_cgroups(
    label: &str,
    allow_failure: bool,
    scope_runtime_s: Option<i64>,
    verify_runtime: bool,
) -> Result<BoxedCgroups, u8> {
    if !is_in_scope() {
        if allow_failure {
            return unavailable(
                label,
                true,
                "cgroup boxing was not established; process-group teardown and per-step wall limits remain active",
            );
        }
        let attempt = attempt_scope_reexec(None, None, scope_runtime_s);
        return unavailable(
            label,
            false,
            &format!(
                "cgroup boxing could not be established: {}; resource boxing is required",
                attempt.describe()
            ),
        );
    }

    let attempt = attempt_scope_reexec(None, None, None);
    let Some(memory_max) = expected_outer_memory_max_bytes() else {
        return unavailable(
            label,
            allow_failure,
            "the outer scope did not carry its requested MemoryMax",
        );
    };
    let outer_limits_observed = outer_scope_limits_observed(attempt.proof(), memory_max);
    // Capture ownership BEFORE Cgroups::new() moves this process into a local
    // `supervisor` child. After that move, path shape alone cannot distinguish
    // a scope-level owner from a scheduler payload's nested supervisor.
    let owns_outer_scope = attempt.proof().is_some_and(invocation_owns_promised_scope);
    let runtime_observed =
        !verify_runtime || expected_scope_runtime_max_s().is_some_and(verify_scope_runtime_max);
    let manager = Cgroups::new();
    let observations = ScopeObservations {
        live_containment: attempt.is_contained(),
        outer_memory_swap_and_oom_group: outer_limits_observed,
        runtime_max: runtime_observed,
        per_step_cgroups: manager.enabled(),
    };
    if let Err(message) = require_scope_observations(observations, verify_runtime) {
        let detail = if !observations.live_containment {
            format!("{message}: {}", attempt.describe())
        } else {
            message.to_string()
        };
        return unavailable(label, allow_failure, &detail);
    }
    let placement = establish_cpu_placement(label, attempt.proof(), owns_outer_scope);
    let _ = CPU_PLACEMENT.set(placement);
    if owns_outer_scope {
        install_scope_teardown();
    } else {
        eprintln!(
            "{label}: inherited outer scope; this nested invocation will not install the outer \
             SIGINT/SIGTERM teardown handler."
        );
    }
    eprintln!(
        "{label}: cgroup boxing ACTIVE; containment and outer limits OBSERVED: {}.",
        attempt.describe()
    );
    Ok(Some(Arc::new(manager) as Arc<dyn CgroupManager>))
}

/// Inert two-sided checks for the decisions made from live cgroup observations.
pub fn self_test() -> Result<String, String> {
    let all = ScopeObservations {
        live_containment: true,
        outer_memory_swap_and_oom_group: true,
        runtime_max: true,
        per_step_cgroups: true,
    };
    require_scope_observations(all, true).map_err(str::to_owned)?;
    for (requirement, observations) in [
        (
            ScopeRequirement::LiveContainment,
            ScopeObservations {
                live_containment: false,
                ..all
            },
        ),
        (
            ScopeRequirement::OuterMemorySwapAndOomGroup,
            ScopeObservations {
                outer_memory_swap_and_oom_group: false,
                ..all
            },
        ),
        (
            ScopeRequirement::RuntimeMax,
            ScopeObservations {
                runtime_max: false,
                ..all
            },
        ),
        (
            ScopeRequirement::PerStepCgroups,
            ScopeObservations {
                per_step_cgroups: false,
                ..all
            },
        ),
    ] {
        let refused = require_scope_observations(observations, true)
            .expect_err("a missing required observation must refuse");
        if refused != requirement_message(requirement) {
            return Err(format!(
                "cgroup requirement {requirement:?} refused with the wrong reason: {refused}"
            ));
        }
    }
    // The production helper converts a missing observation into Err(3) on the
    // default fail-closed path; accepting Ok(None) is reserved for the explicit
    // allow-failure diagnostic mode.
    if !matches!(
        unavailable("safe-ci scope self-test", false, "planted refusal"),
        Err(3)
    ) {
        return Err("a required observation did not propagate as fail-closed exit 3".into());
    }
    if !matches!(
        unavailable("safe-ci scope self-test", true, "planted refusal"),
        Ok(None)
    ) {
        return Err("explicit allow-failure mode did not remain an unboxed warning".into());
    }
    if !matches!(propagate_result(Err(3)), Err(3)) {
        return Err("the caller-facing helper result did not preserve fail-closed exit 3".into());
    }
    if !runtime_readback_satisfies(false, false)
        || !runtime_readback_satisfies(true, true)
        || runtime_readback_satisfies(true, false)
    {
        return Err(
            "RuntimeMax readback did not distinguish an inherited scope from an invocation-owned request"
                .into(),
        );
    }

    cpu_placement_self_test()?;

    // Explicit-path bracket for the topology that failed in production. The
    // fake `step-child` is below the promised scope, just like nested validate.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "safe-ci-scope-self-test-{}-{nonce}",
        std::process::id()
    ));
    let fixture_result = (|| -> Result<(), String> {
        let scope = tmp.join("fixture.scope");
        let child = scope.join("step-child");
        fs::create_dir_all(&child)
            .map_err(|error| format!("cannot create scope fixture: {error}"))?;
        fs::write(scope.join("memory.max"), "104857600\n")
            .map_err(|error| format!("cannot write memory.max fixture: {error}"))?;
        fs::write(scope.join("memory.swap.max"), "0\n")
            .map_err(|error| format!("cannot write memory.swap.max fixture: {error}"))?;
        fs::write(scope.join("memory.oom.group"), "0\n")
            .map_err(|error| format!("cannot write memory.oom.group fixture: {error}"))?;
        let proof = ContainmentProof {
            cgroup: child,
            pid: std::process::id(),
            unit: Some("fixture.scope".into()),
        };
        if promised_scope_ancestor(&proof).as_deref() != Some(scope.as_path()) {
            return Err("a step child did not resolve its exact promised scope ancestor".into());
        }
        let exact_owner = ContainmentProof {
            cgroup: scope.clone(),
            ..proof.clone()
        };
        let scope_supervisor = ContainmentProof {
            cgroup: scope.join("supervisor"),
            ..proof.clone()
        };
        let nested_supervisor = ContainmentProof {
            cgroup: proof.cgroup.join("supervisor"),
            ..proof.clone()
        };
        let mut no_promise = exact_owner.clone();
        no_promise.unit = None;
        if !invocation_owns_promised_scope(&exact_owner) {
            return Err("the exact promised scope did not retain outer teardown ownership".into());
        }
        if expecting_refusal(|| {
            invocation_owns_promised_scope(&proof)
                || invocation_owns_promised_scope(&scope_supervisor)
                || invocation_owns_promised_scope(&nested_supervisor)
                || invocation_owns_promised_scope(&no_promise)
        }) {
            return Err(
                "an inherited or unpromised topology claimed outer-scope teardown ownership".into(),
            );
        }
        if !outer_scope_limits_observed(Some(&proof), 104857600) {
            return Err("matching ancestor limits were refused".into());
        }

        let mut missing = proof.clone();
        missing.unit = Some("missing.scope".into());
        if expecting_refusal(|| outer_scope_limits_observed(Some(&missing), 104857600)) {
            return Err("a missing promised scope ancestor was accepted".into());
        }
        let mut partial = proof.clone();
        partial.unit = Some("fixture".into());
        if expecting_refusal(|| outer_scope_limits_observed(Some(&partial), 104857600)) {
            return Err("a partial promised scope name was accepted".into());
        }
        fs::write(scope.join("memory.max"), "104849408\n")
            .map_err(|error| format!("cannot mutate memory.max fixture: {error}"))?;
        if expecting_refusal(|| outer_scope_limits_observed(Some(&proof), 104857600)) {
            return Err("a memory.max mismatch was accepted".into());
        }
        fs::remove_file(scope.join("memory.max"))
            .map_err(|error| format!("cannot remove memory.max fixture: {error}"))?;
        fs::create_dir(scope.join("memory.max"))
            .map_err(|error| format!("cannot make memory.max unreadable: {error}"))?;
        if expecting_refusal(|| outer_scope_limits_observed(Some(&proof), 104857600)) {
            return Err("an unreadable memory.max was accepted".into());
        }
        fs::remove_dir(scope.join("memory.max"))
            .map_err(|error| format!("cannot remove unreadable memory.max fixture: {error}"))?;
        fs::write(scope.join("memory.max"), "104857600\n")
            .map_err(|error| format!("cannot restore memory.max fixture: {error}"))?;
        fs::write(scope.join("memory.swap.max"), "1\n")
            .map_err(|error| format!("cannot mutate memory.swap.max fixture: {error}"))?;
        if expecting_refusal(|| outer_scope_limits_observed(Some(&proof), 104857600)) {
            return Err("a nonzero outer swap limit was accepted".into());
        }
        fs::remove_file(scope.join("memory.swap.max"))
            .map_err(|error| format!("cannot remove memory.swap.max fixture: {error}"))?;
        fs::create_dir(scope.join("memory.swap.max"))
            .map_err(|error| format!("cannot make memory.swap.max unreadable: {error}"))?;
        if expecting_refusal(|| outer_scope_limits_observed(Some(&proof), 104857600)) {
            return Err("an unreadable memory.swap.max was accepted".into());
        }
        fs::remove_dir(scope.join("memory.swap.max")).map_err(|error| {
            format!("cannot remove unreadable memory.swap.max fixture: {error}")
        })?;
        fs::write(scope.join("memory.swap.max"), "0\n")
            .map_err(|error| format!("cannot restore memory.swap.max fixture: {error}"))?;
        fs::write(scope.join("memory.oom.group"), "0\n")
            .map_err(|error| format!("cannot mutate memory.oom.group fixture: {error}"))?;
        if expecting_refusal(|| outer_scope_limit_readback_matches(&scope, 104857600)) {
            return Err("a zero outer OOM-group readback was accepted".into());
        }
        fs::remove_file(scope.join("memory.oom.group"))
            .map_err(|error| format!("cannot remove OOM-group fixture: {error}"))?;
        fs::create_dir(scope.join("memory.oom.group"))
            .map_err(|error| format!("cannot plant unwritable OOM-group fixture: {error}"))?;
        if expecting_refusal(|| outer_scope_limits_observed(Some(&proof), 104857600)) {
            return Err("an unwritable outer OOM-group control was accepted".into());
        }
        Ok(())
    })();
    let cleanup_result = fs::remove_dir_all(&tmp)
        .map_err(|error| format!("cannot clean scope fixture {}: {error}", tmp.display()));
    fixture_result?;
    cleanup_result?;
    Ok(
        "safe-ci scope: containment, promised-scope ancestor, outer memory/swap/OOM-group, optional RuntimeMax, per-step cgroups, and CPU placement verdict bracketed"
            .into(),
    )
}

/// Two-sided brackets for the CPU-list parser and the APPLIED verdict.
fn cpu_placement_self_test() -> Result<(), String> {
    let set = |cpus: &[u32]| cpus.iter().copied().collect::<BTreeSet<u32>>();
    // The ceiling first: an unbounded parser would accept `8192` at once, and
    // would try to build a four-billion-element set on the last case.
    for (text, expected) in [
        ("8192", None),
        ("0-8192", None),
        ("8191", Some(set(&[8191]))),
        ("0-4294967295", None),
        ("", Some(set(&[]))),
        ("0", Some(set(&[0]))),
        ("1-3,8\n", Some(set(&[1, 2, 3, 8]))),
        ("1-15,17-31", Some((1..=15).chain(17..=31).collect())),
        ("3-1", None),
        ("0-", None),
        ("0,,1", None),
        ("x", None),
        ("-1", None),
        ("0 1", None),
    ] {
        if parse_cpu_list(text) != expected {
            return Err(format!(
                "CPU list {text:?} parsed as {:?}, expected {expected:?}",
                parse_cpu_list(text)
            ));
        }
    }
    for (cpus, expected) in [
        (set(&[]), ""),
        (set(&[0]), "0"),
        (set(&[1, 2, 3, 5, 7, 8]), "1-3,5,7-8"),
        ((1..=15).chain(17..=315).collect(), "1-15,17-315"),
    ] {
        let text = format_cpu_list(&cpus);
        if text != expected || parse_cpu_list(&text) != Some(cpus.clone()) {
            return Err(format!(
                "CPU set {cpus:?} formatted as {text:?}, expected {expected:?}"
            ));
        }
    }
    let placed = |status, excluded: Option<&str>| CpuPlacementObservation {
        status,
        source: Some("sysfs-pmu-cpumask".into()),
        excluded_cpus: excluded.map(str::to_string),
        requested_allowed_cpus: Some("1-15,17-315".into()),
        scope_unit: Some("dagrun-x.scope".into()),
        scope_cpuset_effective: Some("1-15,17-315".into()),
        cpus_allowed_list: Some("1-15,17-315".into()),
        detail: "why".into(),
    };
    for (observation, expected) in [
        (
            Some(placed("applied", Some("0,16"))),
            "CPU placement: applied (excluded CPUs: 0,16; source: sysfs-pmu-cpumask; \
             Cpus_allowed_list: 1-15,17-315): why; informational, not part of the verdict",
        ),
        (
            Some(placed("not-applied", Some("0,16"))),
            "CPU placement: not-applied (excluded CPUs: 0,16; source: sysfs-pmu-cpumask; \
             Cpus_allowed_list: 1-15,17-315): why; informational, not part of the verdict",
        ),
        (
            Some(placed("unrestricted", Some(""))),
            "CPU placement: unrestricted (excluded CPUs: none; source: sysfs-pmu-cpumask; \
             Cpus_allowed_list: 1-15,17-315): why; informational, not part of the verdict",
        ),
        (
            Some(placed("not-requested", None)),
            "CPU placement: not-requested (excluded CPUs: not set; source: sysfs-pmu-cpumask; \
             Cpus_allowed_list: 1-15,17-315): why; informational, not part of the verdict",
        ),
        (
            None,
            "CPU placement: not observed (this invocation never resolved its cgroups, so no \
             placement was applied or inherited); informational, not part of the verdict",
        ),
    ] {
        let line = cpu_placement_summary_line(observation.as_ref());
        if line != expected {
            return Err(format!(
                "placement summary line {line:?}, expected {expected:?}"
            ));
        }
    }
    let requested = set(&[1, 2, 3, 5]);
    if let Err(reason) = cpu_placement_verdict(&requested, Some("1-3,5"), Some("1-3,5")) {
        return Err(format!("an exact placement was refused: {reason}"));
    }
    if let Err(reason) = cpu_placement_verdict(&requested, Some("1-3,5"), Some("2")) {
        return Err(format!(
            "a narrower affinity inside the allowed set was refused: {reason}"
        ));
    }
    for (effective, affinity, why) in [
        (
            Some("0-5"),
            Some("1-3,5"),
            "a scope cpuset still containing CPU 0",
        ),
        (
            Some("1-3,5"),
            Some("0-5"),
            "an affinity still containing CPU 0",
        ),
        (None, Some("1-3,5"), "an unreadable scope cpuset"),
        (Some("1-3,5"), None, "an unreadable affinity"),
        (Some(""), Some("1-3,5"), "an empty scope cpuset"),
        (Some("1-x"), Some("1-3,5"), "a malformed scope cpuset"),
    ] {
        if cpu_placement_verdict(&requested, effective, affinity).is_ok() {
            return Err(format!("{why} was reported as APPLIED"));
        }
    }
    Ok(())
}
