/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The typed x86-64-v3 host-fact refusal of one finished Hermit run,
//! shared by the integration-test harnesses that convert it into a
//! counted skip. A host fact is a refusal Hermit itself treats as a
//! fact of the host (`cacheable_refusal` in
//! hermit-cli/src/cpu_profile_admission.rs): the FPU-reload XINUSE
//! instability and the XCR0 coverage/pairing sentences, plus the
//! kernel floor checked before the probe. Load- or timing-dependent
//! refusals (interrupt-counter calibration, an inconclusive cell,
//! the wall deadline, a lost PMU counter, I/O or peer failures, the
//! repetitions and hypervisor-exit instabilities) are never host
//! facts and never qualify. The Buck shell-test runner reaches
//! this same predicate through `host_fact_v3_verdict.rs` beside it.

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;

/// The bead a skip names: XSAVE-family interception is the
/// capability that would let the case run on a refusing host.
/// (No KVM bead is emitted: an explicit v3 profile on KVM is refused
/// by request checking, never by the host-fact probe, so no launch
/// this detector qualifies is a KVM one.)
pub const SKIP_ISSUE_XSAVE: &str = "dh-38lkn5";
/// The panic payload a harness abort on a host-fact refusal carries,
/// so the runner can tell the refusal that ended the case from a
/// marker recorded beside an unrelated later failure.
pub const SKIP_ABORT_PREFIX: &str = "HERMIT-TEST-SKIP-ABORT: ";

/// The exit status the abort leaves by: outside Hermit's own codes
/// (0 guest, 1 divergence, 122 policy refusal, 125 internal, 126/127
/// guest faults) and libtest's 101, so the runner can require it
/// exactly and no catch_unwind can continue past it.
/// run_node.py's V3_SKIP_ABORT_EXIT mirrors this value.
pub const SKIP_ABORT_EXIT: i32 = 86;

const REFUSAL_PREFIX: &str = "--cpu-profile x86-64-v3 refused: ";
const FPU_RELOAD_PREFIX: &str = "XINUSE is not stable across a host FPU reload:";
const XCR0_FORMS: &[&str] = &[
    "enables components the XINUSE probe does not cover",
    "does not enable x87, SSE and AVX",
    "enables only some of the AVX-512 components",
    "enables only some of the MPX components",
    "enables MPX but CPUID leaf 0xD leaves no room",
    "enables PKRU but the kernel has not enabled protection keys",
];

const REUSED_MARKER: &str = " (reused refusal: measured by another Hermit process on this host ";

/// The original refusal reason under the probe's reuse marker, or
/// the reason itself when unmarked. Every reuse line is built by
/// `reused_refusal` in hermit-cli/src/xstate_probe/mod.rs as
/// `{reason} (reused refusal: measured by another Hermit process on
/// this host {age}s ago and reused for up to {bound}s instead of
/// measuring again)`, optionally followed by the host-naming suffix
/// admission appends, so the original reason is always present
/// verbatim before the marker. Any other shape containing the
/// marker text — including a marker with no reason before it —
/// fails closed as `None`: the run's refusal is then a named
/// failure, never a skip.
fn core_reason(reason: &str) -> Option<&str> {
    let Some((core, tail)) = reason.split_once(REUSED_MARKER) else {
        if reason.contains("(reused refusal:") {
            return None;
        }
        return Some(reason);
    };
    if core.is_empty() {
        return None;
    }
    let (age, tail) = tail.split_once("s ago and reused for up to ")?;
    if age.is_empty() || !age.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (bound, tail) = tail.split_once("s instead of measuring again)")?;
    if bound.is_empty() || !bound.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !tail.is_empty() && !tail.starts_with(" (host CPU ") {
        return None;
    }
    Some(core)
}

/// Whether `reason` (the text after `--cpu-profile x86-64-v3
/// refused: `) is a host fact, in the exact producer forms Hermit
/// caches: the FPU-reload instability (any XSTATE_BV pair or
/// component suffix), an `XCR0 0x…` coverage/pairing sentence, or
/// the kernel floor. A refusal reused from another Hermit process
/// on this host is classified by its original reason
/// ([`core_reason`]): the probe publishes every refusal it
/// measures — calibration included — for reuse within 60 s, so a
/// reused calibration refusal stays transient while a reused
/// FPU-reload refusal is the same host fact the measuring process
/// saw.
pub fn is_host_fact_reason(reason: &str) -> bool {
    let Some(reason) = core_reason(reason) else {
        return false;
    };
    if reason.starts_with(FPU_RELOAD_PREFIX) {
        return true;
    }
    if reason.starts_with("Linux ") && reason.contains(" predates the 5.16 xstate queries") {
        return true;
    }
    let Some(rest) = reason.strip_prefix("XCR0 0x") else {
        return false;
    };
    let Some((_, sentence)) = rest.split_once(' ') else {
        return false;
    };
    XCR0_FORMS.iter().any(|form| sentence.starts_with(form))
}

/// The first host-fact refusal reason of one finished command,
/// whose stderr may hold several Hermit runs (a shell script under
/// one runner), or `None` unless every refusal in it qualifies:
/// the command exited with the policy-refusal status, there is at
/// least one classification line, every classification line is
/// answered by exactly one `--cpu-profile x86-64-v3 refused:`
/// reason (equal counts), and every such reason is a host fact
/// ([`is_host_fact_reason`]). A skid-overshoot or other `cause=`
/// refusal, a `HERMIT_*` failure marker, a panic, an unanswered or
/// unclassified refusal, or any non-host-fact reason anywhere in
/// the output never qualifies, so a later refusal cannot hide
/// behind an earlier host fact; repeated host-fact refusals (two
/// Hermit processes) qualify by the first reason.
pub fn host_fact_refusal(code: Option<i32>, stderr: &str) -> Option<&str> {
    if code != Some(HERMIT_POLICY_REFUSAL_EXIT) {
        return None;
    }
    let mut first_refusal = None;
    let mut classifications = 0usize;
    let mut refusals = 0usize;
    for line in stderr.lines() {
        let line = line.trim_end();
        if line.starts_with("HERMIT_POLICY_REFUSAL class=policy-refusal cause=") {
            return None;
        }
        if line == "HERMIT_POLICY_REFUSAL class=policy-refusal" {
            classifications += 1;
        } else if line.starts_with("HERMIT_") || line.contains("panicked at") {
            return None;
        }
        if let Some((_, reason)) = line.split_once(REFUSAL_PREFIX) {
            if !is_host_fact_reason(reason) {
                return None;
            }
            refusals += 1;
            if first_refusal.is_none() {
                first_refusal = Some(reason);
            }
        }
    }
    if classifications == 0 || refusals != classifications {
        return None;
    }
    first_refusal
}

/// The bead for a launch, from its decided facts: an explicit
/// profile other than x86-64-v3 never requests v3; an explicit v3
/// profile or an auto choice both need a launch that can present
/// v3 (`admits`) — explicitly, or through a guest declaring more
/// than x86-64-v2 (`needs`). Pure, so the matrix is unit-tested
/// in the dedicated host_fact_v3 test binary without touching a
/// Hermit invocation.
pub fn bead_for(explicit_profile: Option<&str>, admits: bool, needs: bool) -> Option<&'static str> {
    match explicit_profile {
        Some("x86-64-v3") if admits => Some(SKIP_ISSUE_XSAVE),
        Some(_) => None,
        None if admits && needs => Some(SKIP_ISSUE_XSAVE),
        None => None,
    }
}

/// The environment variables whose presence forbids the counted
/// skip on every path (the Rust harness and the verdict binary
/// alike): v3 explicitly required by the test's configuration, or
/// a test seam that makes Hermit refuse on any host, whose planted
/// texts copy host-fact forms and so cannot be told from a host
/// fact by stderr alone.
pub const REQUIRE_V3_ENV: &str = "HERMIT_TEST_REQUIRE_CPU_PROFILE_V3";
pub const XCR0_SEAM_ENV: &str = "HERMIT_TEST_CPU_PROFILE_XCR0_EXTRA_BITS";
pub const PROBE_REFUSAL_SEAM_ENV: &str = "HERMIT_TEST_XSTATE_PROBE_REFUSAL";

/// Whether the launch environment forbids the counted skip:
/// `REQUIRE_V3_ENV=1` (the case must run or fail by name), or
/// either refusal seam set to any value (the refusal may be
/// seam-made, not this host's qualification). `get` resolves a
/// variable for the launch: the harness passes its command-aware
/// lookup, the verdict binary its own process environment, which
/// its runner sets to the test's. Pure in `get`.
pub fn env_forbids_skip(mut get: impl FnMut(&str) -> Option<String>) -> bool {
    get(REQUIRE_V3_ENV).as_deref() == Some("1")
        || get(XCR0_SEAM_ENV).is_some()
        || get(PROBE_REFUSAL_SEAM_ENV).is_some()
}

/// The `SKIP-ISSUE: <bead>: <reason>` skip-marker reason for a
/// host-fact `refusal` on the path `bead` names, as
/// run_node.py's issue_skip parses it.
pub fn skip_issue_reason(bead: &str, refusal: &str) -> String {
    format!(
        "SKIP-ISSUE: {bead}: host-inapplicable: the case's guest requires \
         the x86-64-v3 virtual CPU and this host refuses to qualify \
         for it ({refusal})"
    )
}

/// The panic payload aborting a case at its refused command, so the
/// refusal is the case's failure and nothing after it can run.
pub fn skip_abort_payload(bead: &str, refusal: &str) -> String {
    format!("{SKIP_ABORT_PREFIX}{bead}: {refusal}")
}
