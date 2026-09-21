/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Widely-shared type definitions.

pub mod backend_engagement;
pub mod build_info;
pub mod host_capability;

/// Exit status for a run HERMIT DELIBERATELY REFUSED, as distinct from one
/// where hermit itself broke.
///
/// ⚠️ "REFUSED" AND "BROKE" DEMAND OPPOSITE RESPONSES, AND THEY WERE THE SAME
/// NUMBER. A fail-closed policy stopping a run is hermit working correctly: the
/// operator must read the refusal and change their program or their flags.
/// `HERMIT_INTERNAL_FAILURE_EXIT` (125) says hermit is broken and the operator
/// must file a bug. Reporting the first as the second sends a reader looking
/// for a defect in a shutdown path that behaved exactly as designed.
///
/// ⚠️ WHY IT IS NOT 1, WHICH IS WHAT THIS PATH USED TO EXIT.
/// `unrecoverable_shutdown` called `std::process::exit(1)`, so the container
/// child exited 1 and the parent's classifier — whose arm is documented "the
/// child died with a status it did not pick" — turned a status hermit HAD
/// picked into `class=container-child-exit` and 125. Restoring 1 would fix the
/// misclassification and reintroduce a worse one: 1 is the commonest guest exit
/// status, so it cannot distinguish "hermit refused" from "your program
/// returned 1".
///
/// ⚠️ WHY 122. It sits immediately below the reserved band (123 safehermit log
/// cap, 124 deadline, 125 hermit broke, 126/127 GNU exec-level), keeping the
/// reserved codes contiguous, and it is the cheapest possible narrowing of the
/// guest range. See the full allocation above the constants in
/// `hermit-cli/src/lib.rs`.
///
/// ⚠️ IT LIVES IN `detcore-model` BECAUSE BOTH SIDES NEED IT. `detcore` emits it
/// and `hermit-cli` recognises it; `detcore-model` is the only crate both
/// depend on. A copy on each side is exactly the defect that left eight cli
/// tests asserting a stale exit status for a day after the product moved.
pub const HERMIT_POLICY_REFUSAL_EXIT: i32 = 122;

// ⚠️ THE VALUE IS PINNED, NOT ONLY NAMED. `tests/cli.rs` and the allocation
// table both assert 122; a one-character edit here would move every consumer
// with it and nothing would fail. 0 is called out separately because it is the
// dangerous drift: at 0 a refusal would report SUCCESS.
const _: () = assert!(
    HERMIT_POLICY_REFUSAL_EXIT == 122,
    "122 keeps the reserved band contiguous below 123/124/125/126/127"
);
const _: () = assert!(
    HERMIT_POLICY_REFUSAL_EXIT != 0,
    "a refusal exiting 0 would report success"
);

/// The shell's base for "killed by signal N": a process killed by signal `N` is
/// conventionally reported as `128 + N`.
pub const SIGNAL_EXIT_BASE: i32 = 128;

/// The status a signal-terminated run reports, `128 + signo`.
///
/// ⚠️ A SIGNAL DEATH IS NOT A POLICY REFUSAL, AND FOR ONE RELEASE IT WAS SPELLED
/// AS ONE. `sigint_instakill` is one of four `unrecoverable_shutdown` callers.
/// The other three are fail-closed policy decisions where
/// `HERMIT_POLICY_REFUSAL_EXIT` is right: hermit examined the run and refused it.
/// This one is an OPERATOR INTERRUPT — hermit refused nothing, somebody pressed
/// Ctrl-C — so reporting it as a refusal tells the reader hermit made a decision
/// it did not make. hermit#2659 moved all four off `exit(1)` together, which was
/// an improvement for all four (they had been reported as `125`, "hermit broke")
/// and correct for only three.
///
/// ⚠️ IT ALSO PUT TWO MEANINGS ON 122, WHICH IS THE DEFECT THIS REMOVES. With the
/// SIGINT path exiting `HERMIT_POLICY_REFUSAL_EXIT`, 122 meant both "a fail-closed
/// policy stopped the run" and "the operator killed it" — a legible number with
/// two conditions behind it, which is the exact family `125` was split up to end.
///
/// ⚠️ WHY THIS IS NOT A GENUINE `WIFSIGNALED` STATUS. The faithful way to honour a
/// signal is restore `SIG_DFL`, unblock, re-raise — and it is measurably
/// unavailable here for the same reason it is unavailable to
/// `on_container_init_stop_signal`: a self-sent signal does not come from an
/// ancestor namespace, so a namespace init discards it exactly like the original
/// and survives. `128 + signo` is the closest honest approximation, and it is the
/// spelling that function already uses — this shares its convention rather than
/// inventing a second one.
pub const fn signal_exit_status(signo: i32) -> i32 {
    SIGNAL_EXIT_BASE + signo
}

/// `128 + SIGINT`. Spelled from the shared helper so it cannot drift from the
/// band, and pinned below so it cannot drift from 130.
pub const HERMIT_SIGINT_DEATH_EXIT: i32 = signal_exit_status(2);

/// The highest signal number this recognises, `SIGRTMAX` on Linux.
const MAX_SIGNO: i32 = 64;

/// Recover the signal from a `128 + signo` status, or `None` if it is not in the
/// band.
///
/// ⚠️ THE PARENT NEEDS THIS BECAUSE THE CHILD CANNOT SEND A REAL SIGNAL STATUS.
/// Without it the classifier's catch-all treats every non-refusal child exit as
/// unaccounted and reports `125`, so making the SIGINT path exit `130` on its own
/// would have moved Ctrl-C from "hermit refused" to "hermit broke" — worse than
/// what it replaced. Emitting the code and recognising the code are one change.
///
/// ⚠️ IT IS DELIBERATELY A BAND AND NOT A SINGLE VALUE. `sigint_instakill` is not
/// the only producer: `on_container_init_stop_signal` already exits
/// `128 + signo` for SIGTERM, SIGINT and SIGHUP (143, 130, 129), so keying on 130
/// alone would leave the other two misreported as internal failures.
///
/// The upper bound is `SIGRTMAX`; above it, a status is a guest's own number and
/// not a signal this can honestly name.
pub const fn signal_from_exit_status(status: i32) -> Option<i32> {
    if status > SIGNAL_EXIT_BASE && status <= SIGNAL_EXIT_BASE + MAX_SIGNO {
        Some(status - SIGNAL_EXIT_BASE)
    } else {
        None
    }
}

// ⚠️ THE ROUND TRIP IS PINNED, INCLUDING THE EDGES. 128 is excluded (there is no
// signal 0) and the refusal code must not be readable as a signal, or the two
// classifier arms would both match and the order would silently decide meaning.
const _: () = assert!(matches!(signal_from_exit_status(130), Some(2)));
const _: () = assert!(matches!(signal_from_exit_status(143), Some(15)));
const _: () = assert!(signal_from_exit_status(SIGNAL_EXIT_BASE).is_none());
// ⚠️ THE UPPER EDGE, WHICH WAS UNPINNED WHILE THIS COMMENT CLAIMED IT WAS NOT.
// agent(hermit-005)'s codex lane proved it vacuous: `MAX_SIGNO` 64 -> 15 left
// every test green, so nothing held the top of the band. 192 is `128 + SIGRTMAX`
// and is the last status that IS a signal; 193 is the first that is not and
// belongs to the guest. A band needs both edges or it has one.
const _: () = assert!(
    matches!(
        signal_from_exit_status(SIGNAL_EXIT_BASE + MAX_SIGNO),
        Some(MAX_SIGNO)
    ),
    "128 + SIGRTMAX is the last status in the band"
);
const _: () = assert!(
    signal_from_exit_status(SIGNAL_EXIT_BASE + MAX_SIGNO + 1).is_none(),
    "above SIGRTMAX a status is the guest's own number, not a signal"
);
const _: () = assert!(
    signal_from_exit_status(HERMIT_POLICY_REFUSAL_EXIT).is_none(),
    "a policy refusal must not also parse as a signal death"
);

// ⚠️ PINNED AGAINST BOTH FAILURE DIRECTIONS. The first pin catches the band
// moving; the second catches the far worse edit, because the whole point of the
// change is that these two values are DIFFERENT. If a refactor ever collapsed
// them the build fails here rather than silently restoring the ambiguity.
const _: () = assert!(
    HERMIT_SIGINT_DEATH_EXIT == 130,
    "128 + SIGINT(2); the shell convention every other tool reports"
);
const _: () = assert!(
    HERMIT_SIGINT_DEATH_EXIT != HERMIT_POLICY_REFUSAL_EXIT,
    "a signal death and a policy refusal must not share a code -- that collision is the defect"
);

pub mod collections;
pub mod config;
pub mod fd;
pub mod futex;
pub mod happens_before;
pub mod pedigree;
pub mod pid;
pub mod procfs;
pub mod schedule;
pub mod summary;
pub mod time;
