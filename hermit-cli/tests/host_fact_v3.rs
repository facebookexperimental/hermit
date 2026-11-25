/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The shared host-fact v3 harness hook's unit tests, in their own
//! binary: tests/common is included by every integration binary, so
//! unit tests living there are listed by every binary that includes
//! it (which the deadline_slice_cap KVM gap ledger refuses: its
//! listed tests must be exactly its own source's). Here they
//! compile and list exactly once. Shared code is reached through
//! hermit_test, the single load of common/hermit_binary.rs below.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

#[cfg(test)]
mod host_fact_v3_tests {
    use crate::hermit_test::host_fact_v3::*;

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
        assert_eq!(host_fact_refusal(Some(122), ""), None);
    }

    #[test]
    fn every_refusal_in_the_output_must_be_a_host_fact() {
        let host_fact = "XCR0 0x67 enables only some of the AVX-512 components";
        let other_host_fact = "XCR0 0x2ef enables only some of the MPX components";
        let calibration = "the interrupts-taken counter failed calibration: saw 232";
        // Two identical host-fact refusals qualify, by the first.
        let pair = format!("{}{}", refused(host_fact), refused(host_fact));
        assert_eq!(host_fact_refusal(Some(122), &pair), Some(host_fact));
        // Two different host-fact refusals qualify too (lead's rule:
        // every refusal a decided host fact, no identity required).
        let differing = format!("{}{}", refused(host_fact), refused(other_host_fact));
        assert_eq!(host_fact_refusal(Some(122), &differing), Some(host_fact));
        // One classification answered by two refusals does not,
        // whether they differ or are identical (no dedup either).
        for second in [other_host_fact, host_fact] {
            let crowded = format!(
                "{}Error: --cpu-profile x86-64-v3 refused: {second}\n",
                refused(host_fact)
            );
            assert_eq!(host_fact_refusal(Some(122), &crowded), None, "{second}");
        }
        // A host fact followed by a non-host-fact refusal does not.
        let mixed = format!("{}{}", refused(host_fact), refused(calibration));
        assert_eq!(host_fact_refusal(Some(122), &mixed), None);
        // An unanswered classification line does not.
        let unanswered = format!(
            "{}HERMIT_POLICY_REFUSAL class=policy-refusal\n",
            refused(host_fact)
        );
        assert_eq!(host_fact_refusal(Some(122), &unanswered), None);
        // Nor does a cause= refusal, another failure marker, or a panic.
        for extra in [
            "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=1\n",
            "HERMIT_INTERNAL_FAILURE class=container-child-panic\n",
            "thread 'main' panicked at src/main.rs:1:1:\n",
        ] {
            let poisoned = format!("{}{extra}", refused(host_fact));
            assert_eq!(host_fact_refusal(Some(122), &poisoned), None, "{extra}");
        }
    }

    #[test]
    fn env_rule_forbids_required_and_seam_made_refusals_only() {
        let none = |_: &str| None;
        assert!(!env_forbids_skip(none));
        for (key, value) in [
            (REQUIRE_V3_ENV, "1"),
            (XCR0_SEAM_ENV, "0"),
            (PROBE_REFUSAL_SEAM_ENV, "fpu-reload"),
        ] {
            assert!(
                env_forbids_skip(|k| (k == key).then(|| value.to_owned())),
                "{key}"
            );
        }
        // REQUIRED set to anything but "1" does not forbid.
        assert!(!env_forbids_skip(
            |k| (k == REQUIRE_V3_ENV).then(|| "0".to_owned())
        ));
    }
}

#[cfg(test)]
mod v3_skip_tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;
    use std::process::Output;

    use crate::hermit_test::abort_on_host_fact_v3_refusal_for;
    use crate::hermit_test::abort_on_host_fact_v3_refusal_with_bead;
    use crate::hermit_test::host_fact_v3;
    use crate::hermit_test::record_v3_skip;
    use crate::hermit_test::v3_skip_abort_core;

    fn refused_output() -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(
                (detcore_model::HERMIT_POLICY_REFUSAL_EXIT) << 8,
            ),
            stdout: Vec::new(),
            stderr: concat!(
                "HERMIT_POLICY_REFUSAL class=policy-refusal\n",
                "Error: --cpu-profile x86-64-v3 refused: XINUSE is not ",
                "stable across a host FPU reload: XSTATE_BV 0x2fd\n",
            )
            .as_bytes()
            .to_vec(),
        }
    }

    // The bead matrix itself (SaBRe-shaped and explicit-profile
    // launches included) is unit-tested as bead_for above, whose
    // tests never touch a Hermit invocation and so stay out of
    // generate.py's CPUID-faulting derivation. What remains here
    // is only the abort's own behavior.

    #[test]
    fn the_core_records_into_a_private_skip_file_only_when_enabled() {
        // lead-014's never-returns property, at the split core: the
        // enabled core records the marker and exits the process, so
        // no in-process test can call it and return; what is asserted
        // here is its recording half against a private file (no
        // process-environment change) and that a disabled core
        // records nothing and does return.
        let sidecar =
            std::env::temp_dir().join(format!("hermit-v3-skip-unit-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sidecar);
        std::fs::write(&sidecar, "").unwrap();
        v3_skip_abort_core(
            "unit",
            host_fact_v3::SKIP_ISSUE_XSAVE,
            "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd",
            false,
            Some(&sidecar),
        );
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "");
        let payload = record_v3_skip(
            "unit",
            host_fact_v3::SKIP_ISSUE_XSAVE,
            "XINUSE is not stable across a host FPU reload: XSTATE_BV 0x2fd",
            Some(&sidecar),
        );
        assert!(
            payload.starts_with("HERMIT-TEST-SKIP-ABORT: dh-38lkn5:"),
            "{payload}"
        );
        let recorded = std::fs::read_to_string(&sidecar).unwrap();
        assert!(
            recorded.starts_with("HERMIT-TEST-SKIP: unit\tSKIP-ISSUE: dh-38lkn5:"),
            "{recorded}"
        );
        let _ = std::fs::remove_file(&sidecar);
    }

    #[test]
    fn the_real_abort_records_and_exits_86_in_a_child() {
        // The real abort path, in a child of this test binary with
        // the runner enable and a private skip file on ITS command
        // environment only (no process-env change here): exit 86,
        // the marker in that file, the payload on stderr. The
        // disabled-core case above covers hook inertness; this
        // environment's own enable state is irrelevant by design.
        const CHILD: &str = "HERMIT_V3_SKIP_ABORT_CHILD";
        if std::env::var_os(CHILD).is_some() {
            abort_on_host_fact_v3_refusal_with_bead(
                "unit",
                host_fact_v3::SKIP_ISSUE_XSAVE,
                &refused_output(),
            );
            return;
        }
        let sidecar =
            std::env::temp_dir().join(format!("hermit-v3-skip-child-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&sidecar);
        std::fs::write(&sidecar, "").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["the_real_abort_records_and_exits_86_in_a_child"])
            .env(CHILD, "1")
            .env("HERMIT_RUN_NODE_SKIP_ISSUES", "1")
            .env("HERMIT_TEST_SKIP_FILE", &sidecar)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(host_fact_v3::SKIP_ABORT_EXIT),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("HERMIT-TEST-SKIP-ABORT: dh-38lkn5:"),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let recorded = std::fs::read_to_string(&sidecar).unwrap();
        assert!(recorded.contains("SKIP-ISSUE: dh-38lkn5:"), "{recorded}");
        let _ = std::fs::remove_file(&sidecar);
    }

    #[test]
    fn a_non_refusal_returns_normally() {
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        abort_on_host_fact_v3_refusal_with_bead("unit", host_fact_v3::SKIP_ISSUE_XSAVE, &output);
    }

    #[test]
    fn reused_refusals_split_by_host_fact_in_a_child() {
        // The conversion used by harnesses that already know their
        // guest program (abort_on_host_fact_v3_refusal_for): a
        // planted reused host-fact refusal records the counted skip
        // and exits 86 in a child, while a planted reused transient
        // (calibration) refusal returns normally so the case fails
        // on its own assertions. A copy that skipped both would
        // exit 86 in the transient child and fail this test.
        const CHILD: &str = "HERMIT_V3_SKIP_FOR_CHILD";
        if let Ok(mode) = std::env::var(CHILD) {
            let reason = if mode == "host-fact" {
                "XINUSE is not stable across a host FPU reload: XSTATE_BV \
                 0x2fd against 0x2ff (reused refusal: measured by another \
                 Hermit process on this host 3s ago and reused for up to \
                 60s instead of measuring again)"
            } else if mode == "did-not-finish" {
                // A deadline refusal stays transient before and after
                // the gate-v6 decided-verdict consolidation.
                "the XINUSE probe did not finish within 120 seconds \
                 (reused refusal: measured by another Hermit process \
                 on this host 3s ago and reused for up to 60s instead of \
                 measuring again)"
            } else {
                "the interrupts-taken counter failed calibration: the \
                 counter saw 232 interrupts but the CPU took only 220 in \
                 all (reused refusal: measured by another Hermit process \
                 on this host 3s ago and reused for up to 60s instead of \
                 measuring again)"
            };
            let output = Output {
                status: std::process::ExitStatus::from_raw(
                    detcore_model::HERMIT_POLICY_REFUSAL_EXIT << 8,
                ),
                stdout: Vec::new(),
                stderr: format!(
                    "HERMIT_POLICY_REFUSAL class=policy-refusal\n\
                     Error: --cpu-profile x86-64-v3 refused: {reason}\n"
                )
                .into_bytes(),
            };
            // Drive the entry point each harness shape uses: the
            // program-deciding `_for` form, or the Command-shaped
            // with_bead form the strict-verify call site ships.
            if std::env::var("HERMIT_V3_SKIP_FOR_CHILD_VIA").as_deref() == Ok("with_bead") {
                abort_on_host_fact_v3_refusal_with_bead(
                    "unit",
                    host_fact_v3::SKIP_ISSUE_XSAVE,
                    &output,
                );
            } else {
                abort_on_host_fact_v3_refusal_for(
                    "unit",
                    None,
                    true,
                    output.status.code(),
                    &String::from_utf8_lossy(&output.stderr),
                );
            }
            return;
        }
        for (mode, want_skip) in [
            ("host-fact", true),
            ("transient", false),
            ("did-not-finish", false),
        ] {
            for via in ["for", "with_bead"] {
                let sidecar = std::env::temp_dir().join(format!(
                    "hermit-v3-skip-for-{mode}-{via}-{}.txt",
                    std::process::id()
                ));
                let _ = std::fs::remove_file(&sidecar);
                std::fs::write(&sidecar, "").unwrap();
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["reused_refusals_split_by_host_fact_in_a_child"])
                    .env(CHILD, mode)
                    .env("HERMIT_V3_SKIP_FOR_CHILD_VIA", via)
                    .env_remove(host_fact_v3::REQUIRE_V3_ENV)
                    .env_remove(host_fact_v3::XCR0_SEAM_ENV)
                    .env_remove(host_fact_v3::PROBE_REFUSAL_SEAM_ENV)
                    .env("HERMIT_RUN_NODE_SKIP_ISSUES", "1")
                    .env("HERMIT_CPU_PROFILE", "x86-64-v3")
                    .env("HERMIT_TEST_SKIP_FILE", &sidecar)
                    .output()
                    .unwrap();
                let recorded = std::fs::read_to_string(&sidecar).unwrap();
                if want_skip {
                    assert_eq!(
                        output.status.code(),
                        Some(host_fact_v3::SKIP_ABORT_EXIT),
                        "{mode} stderr:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    assert!(
                        recorded.contains("SKIP-ISSUE: dh-38lkn5:"),
                        "{mode}: {recorded}"
                    );
                } else {
                    assert_eq!(output.status.code(), Some(0), "{mode} via {via}");
                    assert_eq!(recorded, "", "{mode} via {via}");
                }
                let _ = std::fs::remove_file(&sidecar);
            }
        }
    }

    #[test]
    fn a_seam_in_the_child_environment_blocks_the_abort() {
        // The harness-side call of the shared rule: with the
        // forced-refusal seam set in the child's own environment,
        // the same refused output that aborts with 86 above must
        // return normally and record nothing. Reverting
        // v3_skip_enabled to inline checks turns this red.
        const CHILD: &str = "HERMIT_V3_SKIP_SEAM_CHILD";
        const MARKER: &str = "HERMIT_V3_SKIP_SEAM_CHILD_MARKER";
        if std::env::var_os(CHILD).is_some() {
            abort_on_host_fact_v3_refusal_with_bead(
                "unit",
                host_fact_v3::SKIP_ISSUE_XSAVE,
                &refused_output(),
            );
            // Reached only if the abort did not fire, so the parent
            // can tell a child that ran this test from one that ran
            // none (both would otherwise exit 0, sidecar empty); a
            // file, because libtest captures stdout on success.
            let marker = std::env::var_os(MARKER).expect("marker path set by parent");
            std::fs::write(marker, "ran\n").unwrap();
            return;
        }
        let sidecar =
            std::env::temp_dir().join(format!("hermit-v3-skip-seam-{}.txt", std::process::id()));
        let marker = std::env::temp_dir().join(format!(
            "hermit-v3-skip-seam-ran-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(&marker);
        std::fs::write(&sidecar, "").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["a_seam_in_the_child_environment_blocks_the_abort"])
            .env(CHILD, "1")
            .env(MARKER, &marker)
            .env("HERMIT_RUN_NODE_SKIP_ISSUES", "1")
            .env_remove(host_fact_v3::REQUIRE_V3_ENV)
            .env_remove(host_fact_v3::XCR0_SEAM_ENV)
            .env(host_fact_v3::PROBE_REFUSAL_SEAM_ENV, "fpu-reload")
            .env("HERMIT_TEST_SKIP_FILE", &sidecar)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Exactly one child test ran, and it reached past the abort.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran\n");
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), "");
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(&marker);
    }
}
