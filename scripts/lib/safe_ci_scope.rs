//! Establish and verify the outer safe-ci scope used by in-process DAG clients.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
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
        if EXPECTED_REFUSAL.load(Ordering::Relaxed) { " expected-refusal:" } else { "" },
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
        "safe-ci scope: containment, promised-scope ancestor, outer memory/swap/OOM-group, optional RuntimeMax, and per-step cgroups bracketed"
            .into(),
    )
}
