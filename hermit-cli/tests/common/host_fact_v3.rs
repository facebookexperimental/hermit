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
//! facts and never qualify.

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

/// The host-fact refusal reason of one finished Hermit run, or
/// `None` unless the run exited with the policy-refusal status,
/// carries Hermit's policy-refusal classification line, and its
/// `--cpu-profile x86-64-v3 refused:` reason is a host fact
/// ([`is_host_fact_reason`]). A skid-overshoot refusal, a refusal
/// beside a second failure marker, or a marker without the exit
/// status never qualifies.
pub fn host_fact_refusal(code: Option<i32>, stderr: &str) -> Option<&str> {
    if code != Some(HERMIT_POLICY_REFUSAL_EXIT) {
        return None;
    }
    let mut refusal = None;
    let mut classified = false;
    for line in stderr.lines() {
        let line = line.trim_end();
        if line.starts_with("HERMIT_POLICY_REFUSAL class=policy-refusal cause=") {
            return None;
        }
        if line == "HERMIT_POLICY_REFUSAL class=policy-refusal" {
            classified = true;
        } else if line.starts_with("HERMIT_") || line.contains("panicked at") {
            return None;
        }
        if let Some((_, reason)) = line.split_once(REFUSAL_PREFIX) {
            if refusal.is_some() {
                return None;
            }
            refusal = Some(reason);
        }
    }
    refusal.filter(|reason| classified && is_host_fact_reason(reason))
}

/// The bead for a launch, from its decided facts: an explicit
/// profile other than x86-64-v3 never requests v3; an explicit v3
/// profile or an auto choice both need a launch that can present
/// v3 (`admits`) — explicitly, or through a guest declaring more
/// than x86-64-v2 (`needs`). Pure, so the matrix is unit-tested
/// here without touching a Hermit invocation.
pub fn bead_for(explicit_profile: Option<&str>, admits: bool, needs: bool) -> Option<&'static str> {
    match explicit_profile {
        Some("x86-64-v3") if admits => Some(SKIP_ISSUE_XSAVE),
        Some(_) => None,
        None if admits && needs => Some(SKIP_ISSUE_XSAVE),
        None => None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(reason: &str) -> String {
        format!(
            "HERMIT_POLICY_REFUSAL class=policy-refusal\n\
             Error: --cpu-profile x86-64-v3 refused: {reason}\n"
        )
    }

    #[test]
    fn host_fact_forms_qualify() {
        for reason in [
            "XINUSE is not stable across a host FPU reload: XSTATE_BV \
             0x2fd against 0x2ff, first difference at save-area byte 512",
            "XINUSE is not stable across a host FPU reload: XSTATE_BV \
             0x2e5 against 0x2e7, first difference at save-area byte 512",
            "XINUSE is not stable across a host FPU reload: XSTATE_BV \
             0xff against 0x2ff, first difference at save-area byte 512",
            "XCR0 0x802e7 enables components the XINUSE probe does not \
             cover (it covers 0x2ff)",
            "XCR0 0x3 does not enable x87, SSE and AVX, which x86-64-v3 needs",
            "XCR0 0x67 enables only some of the AVX-512 components",
            "XCR0 0x2ef enables only some of the MPX components",
            "XCR0 0x2ff enables MPX but CPUID leaf 0xD leaves no room \
             for BNDREGS and BNDCSR in the probe's save area",
            "XCR0 0x2ff enables PKRU but the kernel has not enabled \
             protection keys for it",
            "Linux 5.14 predates the 5.16 xstate queries the profile \
             answers from",
            // A reused FPU-reload refusal is classified by its
            // original reason (the burst form, verbatim).
            "XINUSE is not stable across a host FPU reload: XSTATE_BV \
             0x2fd against 0x2ff, first difference at save-area byte 512 \
             (case XrstorInit of component 1, peer DirtyPkruDiffers, \
             MXCSR 0x3f80) (reused refusal: measured by another Hermit \
             process on this host 1s ago and reused for up to 60s \
             instead of measuring again)",
            // The same, with admission's host-naming suffix appended.
            "XINUSE is not stable across a host FPU reload: XSTATE_BV \
             0x2fd against 0x2ff (reused refusal: measured by another \
             Hermit process on this host 3s ago and reused for up to \
             60s instead of measuring again) (host CPU Intel, \
             microcode 0x7002b01 on cpu0)",
        ] {
            assert_eq!(
                host_fact_refusal(Some(122), &refused(reason)),
                Some(reason),
                "{reason}"
            );
        }
    }

    #[test]
    fn transient_refusals_never_qualify() {
        for reason in [
            "the interrupts-taken counter failed calibration: the counter saw 232",
            // A reused calibration refusal stays transient: reuse
            // covers every measured refusal, host facts or not.
            "the interrupts-taken counter failed calibration: the \
             counter saw 232 interrupts but the CPU took only 220 in \
             all (reused refusal: measured by another Hermit process \
             on this host 56s ago and reused for up to 60s instead of \
             measuring again)",
            // A reuse line without an original reason fails closed.
            "(reused refusal: measured by another Hermit process on \
             this host 1s ago and reused for up to 60s instead of \
             measuring again)",
            // A malformed reuse marker fails closed too.
            "XINUSE is not stable across a host FPU reload: XSTATE_BV \
             0x2fd against 0x2ff (reused refusal: measured by another process)",
            "XINUSE is not stable across repetitions: the reference image changed",
            "XINUSE is not stable across a hypervisor exit: XSTATE_BV 0x2fd",
            "XCR0 changed during admission (0x2ff, then 0x2e7)",
            "the XINUSE probe did not finish within 120 seconds",
        ] {
            assert_eq!(
                host_fact_refusal(Some(122), &refused(reason)),
                None,
                "{reason}"
            );
        }
    }

    #[test]
    fn burst_replay_all_41_qualify_and_calibration_does_not() {
        mod burst {
            // Verbatim v3 refusal reasons recorded 2026-11-22 on a
            // refusing host (8321HC, family 6 model 85, microcode
            // 0x7002b01; calibration rows also from the 0x2000065
            // host): every FPU-reload reason must qualify, every
            // calibration reason must not.
            pub const BURST_FPU_REASONS: &[&str] = &[
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 51s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 51s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 52s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 52s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 52s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 54s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 54s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 1s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 1s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 1s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 1s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 48s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 48s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 49s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 50s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 51s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 52s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 53s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 53s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 54s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 55s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 56s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 57s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 58s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 59s ago and reused for up to 60s instead of measuring again)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80)",
                "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd against 0x2ff, first difference at save-area byte 512 (case XrstorInit of component 1, peer DirtyPkruDiffers, MXCSR 0x3f80) (reused refusal: measured by another Hermit process on this host 12s ago and reused for up to 60s instead of measuring again)",
            ];
            pub const BURST_CAL_REASONS: &[&str] = &[
                "the interrupts-taken counter failed calibration: the counter saw 232 interrupts but the CPU took only 220 in all (reused refusal: measured by another Hermit process on this host 56s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 233 interrupts but the CPU took only 222 in all",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 25s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 26s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 27s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 28s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 29s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 30s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 31s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 32s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 33s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 34s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 35s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 36s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 37s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 38s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 39s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 40s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 41s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 42s ago and reused for up to 60s instead of measuring again)",
                "the interrupts-taken counter failed calibration: the counter saw 263 interrupts but the CPU took only 252 in all (reused refusal: measured by another Hermit process on this host 43s ago and reused for up to 60s instead of measuring again)",
            ];
        }
        assert_eq!(burst::BURST_FPU_REASONS.len(), 41);
        for reason in burst::BURST_FPU_REASONS {
            assert!(is_host_fact_reason(reason), "{reason}");
            assert_eq!(
                host_fact_refusal(Some(122), &refused(reason)),
                Some(*reason),
                "{reason}"
            );
            let named = format!(
                "{reason} (host CPU Intel(R) Xeon(R) Platinum 8321HC CPU \
                 @ 1.40GHz, microcode 0x7002b01 on cpu0)"
            );
            assert!(is_host_fact_reason(&named), "{named}");
        }
        assert!(!burst::BURST_CAL_REASONS.is_empty());
        for reason in burst::BURST_CAL_REASONS {
            assert!(!is_host_fact_reason(reason), "{reason}");
            assert_eq!(host_fact_refusal(Some(122), &refused(reason)), None);
        }
    }

    #[test]
    fn bead_matrix_never_requests_v3_without_admission() {
        // SaBRe-shaped launches (hermit_modes::sabre_*): no bead at
        // any profile, so those cases run and are never v3-skipped.
        for (explicit, needs) in [
            (None, true),
            (None, false),
            (Some("x86-64-v3"), true),
            (Some("x86-64-v3"), false),
            (Some("x86-64-v2"), true),
        ] {
            assert_eq!(bead_for(explicit, false, needs), None, "{explicit:?}");
        }
        // An explicit non-v3 profile is out by the profile itself,
        // whatever the guest declares (the v2-pin row's reason).
        assert_eq!(bead_for(Some("x86-64-v2"), true, true), None);
        assert_eq!(bead_for(Some("host-no-rdrand"), true, true), None);
        assert_eq!(
            bead_for(Some("x86-64-v3"), true, false),
            Some(SKIP_ISSUE_XSAVE)
        );
        assert_eq!(bead_for(None, true, true), Some(SKIP_ISSUE_XSAVE));
        assert_eq!(bead_for(None, true, false), None);
    }

    #[test]
    fn exit_and_classification_are_required() {
        let reason = "XCR0 0x67 enables only some of the AVX-512 components";
        assert_eq!(host_fact_refusal(Some(0), &refused(reason)), None);
        assert_eq!(host_fact_refusal(None, &refused(reason)), None);
        let unclassified = format!("Error: --cpu-profile x86-64-v3 refused: {reason}\n");
        assert_eq!(host_fact_refusal(Some(122), &unclassified), None);
    }
}
