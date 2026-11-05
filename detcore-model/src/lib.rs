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
pub mod canonical_test_epoch;
pub mod cpu_profile;
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
/// ⚠️ WHY 122. It sits immediately below the reserved band (123 log byte cap --
/// hermit's `--max-log-bytes`, and the value reserved for `bin/safehermit`'s
/// log-cap kill, which still exits 125 -- then 124 deadline, 125 hermit broke,
/// 126/127 GNU exec-level), keeping the reserved codes contiguous, and it is
/// the cheapest possible narrowing of the guest range. See the full allocation
/// above the constants in `hermit-cli/src/lib.rs`.
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

/// Attempts a test harness gives a Hermit run refused only for a typed PMU
/// skid overshoot (exit [`HERMIT_POLICY_REFUSAL_EXIT`],
/// `cause=skid-overshoot`): one initial attempt plus four skid retries.
/// In the manifest runner the attempts past the generic retry bound are
/// additionally wall-bounded to that bound's lifecycle, so this count
/// never widens an enclosing node timeout.
///
/// A skid refusal measures the host's PMU interrupt latency under load,
/// not the guest (<https://github.com/rrnewton/hermit/issues/1845>), so its
/// budget is separate from every generic retry budget (nextest's own
/// retries, the manifest runner's per-cell retry). Every harness that
/// retries such a refusal spells the bound from this one constant — the
/// integration-test helper, the manifest runner's skid budget and the
/// nextest CPU wrapper's retry credits (bound − 1) — instead of keeping a
/// copy each. Exhausting it is a distinct infrastructure verdict
/// (`SKID-INFRA`), never a test failure.
///
/// It lives here for the reason [`HERMIT_POLICY_REFUSAL_EXIT`] does: the
/// integration tests, `hermit-verify` and the manifest-plan runner all
/// depend on `detcore-model`, and on no common test crate.
pub const SKID_RETRY_MAX_ATTEMPTS: u32 = 5;

/// The distinct verdict a harness selects when the skid budget
/// ([`SKID_RETRY_MAX_ATTEMPTS`]) is exhausted by typed skid-overshoot
/// refusals and nothing else: the host refused every measurement, so
/// there is no product result and no test failure. Spelled identically
/// in the manifest runner, the nextest counted output and the
/// validate-run verdict built on them.
pub const SKID_INFRA_VERDICT: &str = "SKID-INFRA";

const _: () = assert!(
    SKID_RETRY_MAX_ATTEMPTS == 5,
    "the skid budget is five attempts everywhere; a copy drifting from it is the defect this removes"
);

/// Exit status for a run that DEADLOCKED UNDER HERMIT'S MODEL: Detcore's
/// scheduler found every remaining guest thread blocked on something no later
/// turn can supply (its "Deadlock detected" report) and ended the run.
///
/// ⚠️ A VERDICT OF HERMIT'S MODEL, NOT THE GUEST'S RESULT. "No possible wake"
/// means no wake that Detcore models: under Hermit's sequentialized virtual
/// time "no thread can be woken now" is also "no thread can ever be woken" BY
/// ANYTHING HERMIT MODELS. When the model is complete for the program, Linux
/// would leave it hung forever and the hang is reported instead of reproduced.
/// When it is not, the deadlock is Hermit's bug: the tests record robust-futex
/// owner death, dash's sigsuspend, raw exit, KVM's kill and happens-before
/// re-admission each once reported this way. So the status is neither a guest
/// status nor `HERMIT_INTERNAL_FAILURE_EXIT` (125), and `--verify`, chaos-stress
/// and `analyze` do not take it as a guest result by default. A caller that
/// needs to know which it was compares a native run (`docs/ERROR_CATALOG.md`).
/// Before this, the scheduler exited the container child 1 and the parent's
/// classifier turned the deadlock into `class=container-child-exit` and 125.
///
/// ⚠️ ONLY A GUEST DEADLOCK EXITS THIS. The scheduler's `HERMIT_DEADLOCK:`
/// verdicts (an invariant of Hermit's own bookkeeping broke, or a backend never
/// reported a death the kernel owed) still exit 1 and stay internal failures,
/// and a happens-before refusal still exits [`HERMIT_POLICY_REFUSAL_EXIT`].
///
/// ⚠️ WHY 121. It extends the reserved band downwards by one (121 guest
/// deadlock, 122 refusal, 123 log cap, 124 deadline, 125 hermit broke, 126/127
/// exec-level), so the reserved codes stay contiguous and the guest range loses
/// only its top value. It is not a signal status (`signal_from_exit_status` is
/// `None`), so the classifier arms cannot both match. As with every other code,
/// a guest may still exit 121 of its own accord: the stderr line
/// [`HERMIT_MODEL_DEADLOCK_LINE`] is the discriminator, present only when the
/// scheduler declared the deadlock. See the allocation table above the
/// constants in `hermit-cli/src/lib.rs`.
///
/// It lives here for the reason [`HERMIT_POLICY_REFUSAL_EXIT`] does: `detcore`
/// emits it and `hermit-cli` recognises it.
pub const HERMIT_GUEST_DEADLOCK_EXIT: i32 = 121;

/// The stderr class line Hermit prints, last, for a run that ended with
/// [`HERMIT_GUEST_DEADLOCK_EXIT`]: a deadlock under Hermit's model. Named for
/// the model, not the guest, because the verdict is only as complete as the
/// model (see [`HERMIT_GUEST_DEADLOCK_EXIT`]).
pub const HERMIT_MODEL_DEADLOCK_LINE: &str =
    "HERMIT_MODEL_DEADLOCK class=deadlock-under-hermit-model";

const _: () = assert!(
    HERMIT_GUEST_DEADLOCK_EXIT == 121,
    "121 extends the reserved band 122..=127 downwards and keeps it contiguous"
);
const _: () = assert!(
    HERMIT_GUEST_DEADLOCK_EXIT != 0
        && HERMIT_GUEST_DEADLOCK_EXIT != 1
        && HERMIT_GUEST_DEADLOCK_EXIT != HERMIT_POLICY_REFUSAL_EXIT,
    "a deadlock must not read as success, as the scheduler's old exit 1, or as a refusal"
);

/// Exit status for a run whose virtual clock passed `--virtual-timeout`: 124,
/// the status every deadline in Hermit already uses (`hermit::HERMIT_DEADLINE_EXIT`,
/// which pins itself to this value). The stderr class line, not the status,
/// tells it from `--timeout`'s.
const VIRTUAL_TIMEOUT_EXIT: i32 = 124;

/// How Detcore's scheduler ended a run itself, with a status and a stderr class
/// line of its own: NEITHER KIND IS A GUEST RESULT. A deadlock is a verdict of
/// Hermit's model (see [`HERMIT_GUEST_DEADLOCK_EXIT`]) and a virtual timeout is
/// a bound Hermit imposed, so `--verify` (under the default
/// `--verify-allow=success`), chaos-stress and `analyze` do not take either as
/// the guest's outcome. Neither is a Hermit failure (125) either.
///
/// The scheduler hands it to the run's owner when the backend can unwind the
/// parked guest (ptrace), and otherwise exits the process with
/// [`TerminalOutcome::exit_status`]. Both kinds stop a run at a committed
/// scheduler turn, so both stop it at the same point of the same schedule in
/// every run.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize
)]
pub enum TerminalOutcome {
    /// Every remaining guest thread is blocked with no wake Hermit models: a
    /// deadlock under Hermit's model, not a guest result
    /// ([`HERMIT_GUEST_DEADLOCK_EXIT`], [`HERMIT_MODEL_DEADLOCK_LINE`]).
    GuestDeadlock,
    /// The run's virtual clock passed `--virtual-timeout`: at a committed
    /// scheduler turn, Detcore's global virtual time was more than `bound_ns`
    /// virtual nanoseconds past the run's start (the time a run summary
    /// reports as `virttime_elapsed`). Exits 124, as a deadline does. A bound
    /// Hermit imposed, not a guest result.
    VirtualTimeout {
        /// The configured bound, in virtual nanoseconds.
        bound_ns: u64,
    },
}

impl TerminalOutcome {
    /// The status a run with this outcome ends with.
    pub const fn exit_status(self) -> i32 {
        match self {
            Self::GuestDeadlock => HERMIT_GUEST_DEADLOCK_EXIT,
            Self::VirtualTimeout { .. } => VIRTUAL_TIMEOUT_EXIT,
        }
    }

    /// The machine-readable stderr line Hermit ends such a run with, which
    /// tells it from a guest that exited the same status of its own accord
    /// and, for a virtual timeout, from `--timeout`'s `class=run-timeout`.
    pub fn class_line(self) -> String {
        match self {
            Self::GuestDeadlock => HERMIT_MODEL_DEADLOCK_LINE.to_owned(),
            Self::VirtualTimeout { bound_ns } => {
                format!("HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns={bound_ns}")
            }
        }
    }

    /// The `(guest_deadlock, virtual_timeout_ns)` pair a run summary and a
    /// verification record store an outcome as: a deadlock keeps the
    /// `guest_deadlock` flag it has always been stored as.
    pub fn to_fields(outcome: Option<Self>) -> (bool, Option<u64>) {
        match outcome {
            None => (false, None),
            Some(Self::GuestDeadlock) => (true, None),
            Some(Self::VirtualTimeout { bound_ns }) => (false, Some(bound_ns)),
        }
    }

    /// The outcome [`TerminalOutcome::to_fields`] stored.
    pub fn from_fields(guest_deadlock: bool, virtual_timeout_ns: Option<u64>) -> Option<Self> {
        match (guest_deadlock, virtual_timeout_ns) {
            (true, _) => Some(Self::GuestDeadlock),
            (false, Some(bound_ns)) => Some(Self::VirtualTimeout { bound_ns }),
            (false, None) => None,
        }
    }
}

const _: () = assert!(
    signal_from_exit_status(VIRTUAL_TIMEOUT_EXIT).is_none()
        && VIRTUAL_TIMEOUT_EXIT != HERMIT_GUEST_DEADLOCK_EXIT
        && VIRTUAL_TIMEOUT_EXIT != HERMIT_POLICY_REFUSAL_EXIT,
    "a virtual timeout must not read as a signal death, a deadlock or a refusal"
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
const _: () = assert!(
    signal_from_exit_status(HERMIT_GUEST_DEADLOCK_EXIT).is_none(),
    "a deadlock under Hermit's model must not also parse as a signal death"
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

pub mod access_trace;
pub mod collections;
pub mod config;
pub mod container_mounts;
pub mod fd;
pub mod futex;
pub mod happens_before;
pub mod host_input;
pub mod liteinst_initial_image;
/// Bounded sealed native target catalog data, without admission authority.
#[cfg(feature = "native-exec")]
pub mod liteinst_native_exec_catalog;
pub mod liteinst_native_exec_input;
/// Exact versioned neutral cut envelope outside opaque Tool state.
pub mod liteinst_native_exec_state;
pub mod logdiff_policy;
pub mod network_engine;
pub mod network_trace;
pub mod pedigree;
pub mod pid;
pub mod procfs;
pub mod replay_departure;
pub mod schedule;

/// Source text of `schedule.rs`, for Detcore's global-RPC revision digest test.
///
/// The test in detcore hashes the wire definitions in this file's text;
/// exposing the text from this crate keeps that check working in build
/// systems that cannot read a sibling crate's source file by relative path.
#[doc(hidden)]
pub const SCHEDULE_RS_SOURCE: &str = include_str!("schedule.rs");

pub mod summary;
pub mod syscall_histogram;
pub mod time;
pub mod virtual_timeout;

/// Exact native runtime/dispatch-sequence envelope, separate from admission.
pub mod liteinst_native_exec_runtime;

#[cfg(feature = "native-exec")]
pub mod liteinst_native_exec_sources;

#[cfg(test)]
mod terminal_outcome_tests {
    use super::*;

    /// Each terminal outcome has its own status and class line. A guest
    /// deadlock keeps exactly the ones it had (121, [`HERMIT_MODEL_DEADLOCK_LINE`]); a
    /// virtual timeout exits 124 like every deadline but names its own class
    /// and bound, so `--timeout`'s `class=run-timeout` is never confused with
    /// it. Both survive the summary and record fields they are stored as.
    #[test]
    fn terminal_outcomes_carry_their_status_and_class_line() {
        let deadlock = TerminalOutcome::GuestDeadlock;
        assert_eq!(deadlock.exit_status(), 121);
        assert_eq!(
            deadlock.class_line(),
            "HERMIT_MODEL_DEADLOCK class=deadlock-under-hermit-model"
        );
        let timeout = TerminalOutcome::VirtualTimeout {
            bound_ns: 3_500_000_000,
        };
        assert_eq!(timeout.exit_status(), 124);
        assert_eq!(
            timeout.class_line(),
            "HERMIT_RUN_TIMEOUT class=virtual-timeout bound_ns=3500000000"
        );
        assert_ne!(timeout.class_line(), "HERMIT_RUN_TIMEOUT class=run-timeout");
        for outcome in [None, Some(deadlock), Some(timeout)] {
            let (guest_deadlock, virtual_timeout_ns) = TerminalOutcome::to_fields(outcome);
            assert_eq!(
                TerminalOutcome::from_fields(guest_deadlock, virtual_timeout_ns),
                outcome
            );
        }
        assert_eq!(TerminalOutcome::to_fields(Some(deadlock)), (true, None));
    }
}
