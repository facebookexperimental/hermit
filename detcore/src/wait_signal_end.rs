/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1c: which pending signals end a parked wait under the in-guest
//! delivery gate (`Config::in_guest_signal_delivery`), and what the wait's
//! end tells the restart checkpoint (design D40, D50; dev-hermit
//! ai_docs/2026-10-16-a5-signal-delivery-design.md, "3c.1 implementation
//! plan, revision 4", <https://github.com/rrnewton/hermit/issues/4485>).
//!
//! Once the in-guest runtime runs under a mask of its own (step 3c.1e), the
//! kernel's `SigBlk` for a guest thread is the runtime's, and a signal sent
//! to a parked thread stays pending in the kernel, held by that mask, until
//! Detcore ends the wait. So Detcore decides from kernel truth plus the one
//! fact the guest publishes (design 16.11):
//!
//! - the target's pending and action sets from `/proc/<tid>/status`
//!   (`SigPnd`, `ShdPnd`, `SigCgt`, `SigIgn`);
//! - the guest's mask as the thread published it at its latest request
//!   (step 3c.1b), never `SigBlk`.
//!
//! The deliverable set is `(SigPnd ∪ ShdPnd) ∖ mask ∩ (SigCgt ∪ (default-fatal
//! ∖ SigIgn))`: a signal Linux would deliver at the wait's return, a caught one
//! by running its handler, a default-fatal one by ending the process. A
//! blocked signal stays pending; an ignored or default-ignored one, or a
//! default stop, does not end the wait (stop sends are refused under the gate,
//! D48). The in-guest runtime's reserved signals
//! (`Config::in_guest_reserved_signals`) are left out: their `SigCgt` bit is
//! the runtime's handler, not a guest disposition, and a cross-process send of
//! one is refused (step 3c.0a).

use crate::syscalls::KernelSignalState;
use crate::syscalls::SignalQueue;
use crate::syscalls::kernel_sigset_bit;

/// What ended a wait for a signal under the in-guest delivery gate: D40's
/// per-syscall flag, set by the path that ends the wait
/// (`ThreadState::wait_signal_end`) and taken by the restart checkpoint
/// straight after the dispatch `match` (`Detcore::wait_end_checkpoint`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitSignalEnd {
    /// The process dies at the return and the guest never resumes after the
    /// call: the guest result is `-EINTR`, and the original restart code is
    /// what the happens-before checkpoint records (IR-2). Either a
    /// non-core-dumping default-fatal signal is deliverable (the first such
    /// one Linux would dequeue), or Linux's delivery loop reaches a
    /// core-dumping default-fatal one that no mask blocks ([`classify`],
    /// [`DequeueWalk::resolve`]).
    ///
    /// The signal is not the exit status for a non-core-dumping end: Linux
    /// takes that from `group_exit_code`, the first such signal SENT, which
    /// `complete_signal` records when it starts the group exit
    /// (kernel/signal.c:1041-1049 at v6.12), not the first one dequeued.
    /// Step 3c.1e, which takes the death's status, must use the send order.
    Fatal(i32),
    /// The first signal Linux would dequeue is caught, and Linux's delivery
    /// loop reaches no default-fatal signal: either none is deliverable, or
    /// the masks of the handlers it sets up before reaching one block it,
    /// and no second instance of a caught signal is dequeued under a default
    /// action that `SA_RESETHAND` restored. That first signal's handler runs
    /// at the return, and the call's result goes through the x86 restart
    /// table with that action's flags (`signal_restart::caught_signal_return`).
    Caught(i32),
    /// The first signal Linux would dequeue is caught, and a later delivery
    /// in the same loop might end the process: a core-dumping default-fatal
    /// signal is deliverable behind it, or a caught signal has a second
    /// instance (a standard signal pending on both queues) or may have one (a
    /// real-time signal), which `SA_RESETHAND` would leave at its default
    /// action. The end depends on the caught actions (their `sa_mask` and
    /// `sa_flags`), which the restart checkpoint reads in the thread's own
    /// turn and resolves ([`DequeueWalk::resolve`]).
    BehindCaught(DequeueWalk),
}

/// The parts of a caught signal's action that decide what Linux blocks
/// while its handler is set up, and the restart table's flags: its
/// `sa_mask` and `sa_flags`, as a query `rt_sigaction` reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaughtAction {
    pub(crate) mask: u64,
    pub(crate) flags: u64,
}

/// A delivery loop the checkpoint cannot decide ([`DequeueWalk::resolve`]),
/// which it refuses by name (D49), fail closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkRefusal {
    /// The action of caught signal `caught` could not be read while a later
    /// delivery could still end the process; `behind` is the signal that
    /// could (HERMIT_CAUGHT_ACTION_UNREADABLE).
    ActionUnknown { caught: i32, behind: i32 },
    /// `SA_RESETHAND` reset caught signal `signal` to its default action, a
    /// stop, and a second instance of it is dequeued: the in-guest backend
    /// cannot stop a guest process on the schedule (D48;
    /// HERMIT_SIGNAL_RESET_TO_STOP).
    ResetToStop { signal: i32 },
    /// A caught real-time signal with `SA_RESETHAND` stays unblocked after its
    /// delivery, so another queued instance of it would be dequeued at its
    /// default action, termination; `/proc` shows only whether one instance
    /// is pending, not how many, and Detcore keeps no count of the
    /// physically pending ones (HERMIT_REALTIME_INSTANCES_UNKNOWN).
    RealtimeInstances { signal: i32 },
}

/// The real-time signals, 32 to 64 in the kernel's numbering (`SIGRTMIN`
/// is 32 there; C libraries reserve the first few). Any number of instances
/// of one can be queued (`collect_signal`, kernel/signal.c:566-596 at
/// v6.12).
fn realtime() -> u64 {
    !0 << 31
}

/// The stop signals, whose default action stops the thread group.
fn stops() -> u64 {
    [libc::SIGSTOP, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU]
        .into_iter()
        .fold(0, |set, signal| set | kernel_sigset_bit(signal))
}

/// The input of Linux's delivery loop at a wait's end, for a
/// [`WaitSignalEnd::BehindCaught`]: the deliverable signals on the thread's
/// private queue and on the shared queue (a standard signal on both is two
/// instances), which of them are default-fatal (the rest are caught), and
/// the guest's mask they were found under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DequeueWalk {
    thread: u64,
    shared: u64,
    fatal: u64,
    mask: u64,
}

impl DequeueWalk {
    /// The first instance, in dequeue order, that could still end the
    /// process: unblocked, and default-fatal, or reset to its default action
    /// (`reset`), or a caught real-time signal (it may be queued again), or a
    /// caught standard signal pending on both queues (two instances).
    fn at_risk(&self, thread: u64, shared: u64, blocked: u64, reset: u64) -> Option<i32> {
        let unblocked = (thread | shared) & !blocked;
        let caught = !self.fatal;
        let risky = unblocked & (self.fatal | reset | (caught & (realtime() | (thread & shared))));
        queued(thread, shared, risky)
            .next_dequeued(blocked)
            .map(|(signal, _)| signal)
    }

    /// Linux's delivery loop over this walk (v6.12). At the return to user
    /// mode `exit_to_user_mode_loop` calls `arch_do_signal_or_restart` for as
    /// long as a signal is pending and unblocked (kernel/entry/common.c:
    /// 97-130; arch/x86/kernel/signal.c:333-341), before any user
    /// instruction runs. Each `get_signal` dequeues the next instance in
    /// order (private queue first, then synchronous signals, then the lowest
    /// number; kernel/signal.c:634-639, :196-218):
    ///
    /// - a caught one: `get_signal` resets the action to `SIG_DFL` when it has
    ///   `SA_RESETHAND` (`SA_ONESHOT`, :2821-2822), `handle_signal` sets up its
    ///   frame, and `signal_delivered` (:2941-2954) adds the action's
    ///   `sa_mask`, and the signal itself unless `SA_NODEFER`, to the blocked
    ///   mask; the loop goes on;
    /// - a default-fatal one not blocked by then ends the process
    ///   (:2878-2906) without any handler running: `Fatal`;
    /// - a second instance of a caught signal whose action was reset is
    ///   taken at its default action: termination or a core dump is `Fatal`,
    ///   a default-ignored one is discarded (the loop goes on), a stop is
    ///   [`WalkRefusal::ResetToStop`].
    ///
    /// So `SA_NODEFER` matters exactly when a second instance of the same
    /// signal exists and `SA_RESETHAND` reset it: the legacy `signal()`
    /// installs both (`SA_ONESHOT | SA_NOMASK`, kernel/signal.c:4675). A
    /// standard signal has at most one instance per queue, so one pending on
    /// both queues is two. A real-time signal may have any number, which
    /// `/proc` does not show: a caught one with `SA_RESETHAND` that stays
    /// unblocked after its delivery is [`WalkRefusal::RealtimeInstances`].
    /// With every remaining instance blocked or harmless, the end is
    /// `Caught` with the first caught signal.
    ///
    /// `action` gives a caught signal's action. It is asked only while a
    /// later delivery could still end the process ([`Self::at_risk`], or the
    /// delivered signal is real-time or has another instance); a `None` then
    /// is [`WalkRefusal::ActionUnknown`].
    pub(crate) fn resolve(
        &self,
        mut action: impl FnMut(i32) -> Option<CaughtAction>,
    ) -> Result<WaitSignalEnd, WalkRefusal> {
        let mut blocked = self.mask;
        let mut reset = 0;
        let (mut thread, mut shared) = (self.thread, self.shared);
        let mut first = None;
        while let Some((signal, queue)) = queued(thread, shared, !0).next_dequeued(blocked) {
            let bit = kernel_sigset_bit(signal);
            match queue {
                SignalQueue::Thread => thread &= !bit,
                SignalQueue::Shared => shared &= !bit,
            }
            if reset & bit != 0 {
                if crate::syscalls::default_action_ends_process(0, 0, signal) {
                    return Ok(WaitSignalEnd::Fatal(signal));
                }
                if stops() & bit != 0 {
                    return Err(WalkRefusal::ResetToStop { signal });
                }
                continue;
            }
            if self.fatal & bit != 0 {
                return Ok(WaitSignalEnd::Fatal(signal));
            }
            let first = *first.get_or_insert(signal);
            let again = (thread | shared) & bit != 0 || realtime() & bit != 0;
            let risk = self.at_risk(thread, shared, blocked, reset);
            if risk.is_none() && !again {
                return Ok(WaitSignalEnd::Caught(first));
            }
            let Some(caught) = action(signal) else {
                return Err(WalkRefusal::ActionUnknown {
                    caught: signal,
                    behind: risk.unwrap_or(signal),
                });
            };
            blocked |= caught.mask;
            if caught.flags & libc::SA_NODEFER as u64 == 0 {
                blocked |= bit;
            }
            if caught.flags & libc::SA_RESETHAND as u64 != 0 {
                reset |= bit;
                if realtime() & bit != 0 && blocked & bit == 0 {
                    return Err(WalkRefusal::RealtimeInstances { signal });
                }
            }
        }
        Ok(WaitSignalEnd::Caught(
            first.expect("a walk starts with a deliverable caught signal"),
        ))
    }
}

/// The instances on the private queue `thread` and the shared queue
/// `shared` among `set`, as a state `next_dequeued` reads.
fn queued(thread: u64, shared: u64, set: u64) -> KernelSignalState {
    KernelSignalState {
        pending: (thread | shared) & set,
        thread_pending: thread & set,
        shared_pending: shared & set,
        ..KernelSignalState::default()
    }
}

/// The signals whose default action ends the process (terminate or dump core)
/// and whose disposition is still the default: not caught, not ignored, not a
/// default-ignored signal (`SIGCHLD`, `SIGCONT`, `SIGURG`, `SIGWINCH`), and not
/// a stop. `SIGKILL` always is.
pub(crate) fn default_fatal(state: &KernelSignalState) -> u64 {
    (1..=64).fold(0, |set, signal| {
        if crate::syscalls::default_action_ends_process(state.ignored, state.caught, signal) {
            set | kernel_sigset_bit(signal)
        } else {
            set
        }
    })
}

/// The design's deliverable set for a thread whose guest mask is `mask`: the
/// signals pending for it (`SigPnd ∪ ShdPnd`) that `mask` leaves unblocked and
/// that are caught, or default-fatal and not ignored, less `reserved`, the
/// in-guest runtime's reserved signals.
pub(crate) fn deliverable(state: &KernelSignalState, mask: u64, reserved: u64) -> u64 {
    state.pending & !mask & (state.caught | default_fatal(state)) & !reserved
}

/// What a wait that `deliverable` (a non-empty subset of the design's set for
/// `state` under the guest's mask `mask`) ends means for the call's result,
/// or `None` when nothing ends it, in Linux's order (v6.12):
///
/// 1. A deliverable default-fatal signal that does not dump core makes it
///    `Fatal`: `complete_signal` starts the group exit when such a signal is
///    sent to a thread that wants it, queueing SIGKILL to every thread
///    (kernel/signal.c:1034-1054, its `!sig_kernel_coredump(sig)` branch), so
///    the process dies at the return whatever else is pending. The signal
///    named is the first such one Linux would dequeue (see `Fatal` for the
///    exit status).
/// 2. Otherwise the first deliverable signal in `get_signal`'s dequeue order
///    comes first: `dequeue_signal` takes the thread's private queue before
///    the shared one (kernel/signal.c:634-639), and `next_signal` the
///    synchronous signals first, then the lowest number (:196-218); here
///    `KernelSignalState::next_dequeued`. A core-dumping one is `Fatal`:
///    `get_signal` ends the process for it (:2881-2905). A caught one is
///    `Caught` unless a later delivery in the same loop could end the
///    process: a core-dumping signal deliverable behind it, a deliverable
///    caught standard signal pending on both queues, or a deliverable caught
///    real-time signal (the last two through `SA_RESETHAND`). Then the end
///    depends on the caught actions, so it is `BehindCaught`, which the
///    restart checkpoint resolves ([`DequeueWalk::resolve`]).
pub(crate) fn classify(
    state: &KernelSignalState,
    deliverable: u64,
    mask: u64,
) -> Option<WaitSignalEnd> {
    if deliverable == 0 {
        return None;
    }
    let fatal = deliverable & default_fatal(state);
    let among = |set: u64| KernelSignalState {
        pending: state.pending & set,
        thread_pending: state.thread_pending & set,
        shared_pending: state.shared_pending & set,
        ..*state
    };
    let group_exit = fatal & !core_dumping();
    if group_exit != 0 {
        let (signal, _) = among(group_exit).next_dequeued(0)?;
        return Some(WaitSignalEnd::Fatal(signal));
    }
    let (signal, _) = among(deliverable).next_dequeued(0)?;
    let (thread, shared) = (
        state.thread_pending & deliverable,
        state.shared_pending & deliverable,
    );
    let caught = deliverable & !fatal;
    if fatal & kernel_sigset_bit(signal) != 0 {
        Some(WaitSignalEnd::Fatal(signal))
    } else if fatal != 0 || caught & (realtime() | (thread & shared)) != 0 {
        Some(WaitSignalEnd::BehindCaught(DequeueWalk {
            thread,
            shared,
            fatal,
            mask,
        }))
    } else {
        Some(WaitSignalEnd::Caught(signal))
    }
}

/// The signals whose default action dumps core (`SIG_KERNEL_COREDUMP_MASK`,
/// include/linux/signal.h:426-432 at v6.12).
pub(crate) fn core_dumping() -> u64 {
    [
        libc::SIGQUIT,
        libc::SIGILL,
        libc::SIGTRAP,
        libc::SIGABRT,
        libc::SIGFPE,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGSYS,
        libc::SIGXCPU,
        libc::SIGXFSZ,
    ]
    .into_iter()
    .fold(0, |set, signal| set | kernel_sigset_bit(signal))
}

/// [`deliverable`] then [`classify`].
pub(crate) fn wait_end(
    state: &KernelSignalState,
    mask: u64,
    reserved: u64,
) -> Option<WaitSignalEnd> {
    wait_end_with_virtual(state, mask, reserved, 0)
}

/// [`wait_end`] with `virtual_caught`, caught signals the scheduler knows
/// are pending on the process's shared queue although no physical copy is:
/// signal phase 1's virtual SIGALRM, which Linux's `ITIMER_REAL` and `alarm`
/// queue on the shared queue (`it_real_fn`, kernel/time/itimer.c:156-166 at
/// v6.12) and which the scheduler's ledger delivers only when the guest's
/// mask admits it. They join the deliverable set and take their place in
/// Linux's dequeue order ([`classify`]).
pub(crate) fn wait_end_with_virtual(
    state: &KernelSignalState,
    mask: u64,
    reserved: u64,
    virtual_caught: u64,
) -> Option<WaitSignalEnd> {
    let deliverable = deliverable(state, mask, reserved) | virtual_caught;
    let state = KernelSignalState {
        pending: state.pending | virtual_caught,
        shared_pending: state.shared_pending | virtual_caught,
        caught: state.caught | virtual_caught,
        ignored: state.ignored & !virtual_caught,
        ..*state
    };
    classify(&state, deliverable, mask)
}

/// D44 (A5 step 3c.1c): whether a send of `signal` to a thread blocked in a
/// kernel-run call (`ResourceID::is_kernel_run_blocking`), whose guest mask at
/// that call's request was `mask`, is refused: Linux would deliver it by
/// interrupting the call, which the kernel runs under the in-guest runtime's
/// mask, so it would stay pending and the call would never end for it. That
/// is a signal `mask` leaves unblocked that is caught, or default-fatal and not
/// ignored. A vfork parent (`vfork`) sleeps killable, and Linux wakes it only
/// for a fatal signal that is not core-dumping (`wait_for_vfork_done`,
/// kernel/fork.c:1572-1590; `complete_signal`, kernel/signal.c:1034-1054): a
/// caught or core-dumping signal waits for the child's release, which keeping
/// it pending reproduces, so only an unblocked, non-core-dumping default-fatal
/// signal is refused there. `state` gives the dispositions; its pending set
/// is not read. SIGKILL and signal 0 are never passed here.
pub(crate) fn kernel_run_send_refused(
    state: &KernelSignalState,
    mask: u64,
    signal: i32,
    vfork: bool,
    reserved: u64,
) -> bool {
    if !(1..=64).contains(&signal) {
        return false;
    }
    let bit = kernel_sigset_bit(signal);
    if mask & bit != 0 || reserved & bit != 0 {
        return false;
    }
    if vfork {
        default_fatal(state) & bit != 0 && core_dumping() & bit == 0
    } else {
        (state.caught | default_fatal(state)) & bit != 0
    }
}

/// Whether `errno` is one of the kernel's internal restart codes
/// (include/linux/errno.h v6.12 lines 14-18), which a guest never sees: the
/// kernel either restarts the call or turns the code into `EINTR` at the
/// return.
pub(crate) fn is_restart_code(errno: reverie::Errno) -> bool {
    matches!(
        errno,
        reverie::Errno::ERESTARTSYS
            | reverie::Errno::ERESTARTNOINTR
            | reverie::Errno::ERESTARTNOHAND
            | reverie::Errno::ERESTART_RESTARTBLOCK
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(signal: i32) -> u64 {
        kernel_sigset_bit(signal)
    }

    fn state(thread: u64, shared: u64, ignored: u64, caught: u64) -> KernelSignalState {
        KernelSignalState {
            pending: thread | shared,
            thread_pending: thread,
            shared_pending: shared,
            // The kernel's mask is the in-guest runtime's under the gate;
            // the predicate never reads it.
            blocked: !0,
            ignored,
            caught,
        }
    }

    /// Mutation "SigBlk used": the kernel mask here blocks everything, so a
    /// predicate that read it would find nothing.
    #[test]
    fn a_default_fatal_signal_unblocked_by_the_published_mask_is_deliverable() {
        let s = state(bit(libc::SIGTERM), 0, 0, 0);
        assert_eq!(deliverable(&s, 0, 0), bit(libc::SIGTERM));
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Fatal(libc::SIGTERM))
        );
    }

    #[test]
    fn a_signal_the_published_mask_blocks_is_not_deliverable() {
        let s = state(bit(libc::SIGTERM), 0, 0, 0);
        assert_eq!(deliverable(&s, bit(libc::SIGTERM), 0), 0);
        assert_eq!(wait_end(&s, bit(libc::SIGTERM), 0), None);
    }

    /// Mutation "ignored counted as deliverable".
    #[test]
    fn ignored_and_default_ignored_signals_are_not_deliverable() {
        let s = state(
            bit(libc::SIGTERM) | bit(libc::SIGCHLD) | bit(libc::SIGURG) | bit(libc::SIGWINCH),
            bit(libc::SIGCONT),
            bit(libc::SIGTERM),
            0,
        );
        assert_eq!(deliverable(&s, 0, 0), 0);
    }

    #[test]
    fn default_stops_are_not_deliverable_and_caught_ones_are() {
        let stops = bit(libc::SIGTSTP) | bit(libc::SIGTTIN) | bit(libc::SIGTTOU);
        let s = state(stops, 0, 0, 0);
        assert_eq!(deliverable(&s, 0, 0), 0);
        let caught = state(stops, 0, 0, bit(libc::SIGTSTP));
        assert_eq!(deliverable(&caught, 0, 0), bit(libc::SIGTSTP));
        assert_eq!(
            wait_end(&caught, 0, 0),
            Some(WaitSignalEnd::Caught(libc::SIGTSTP))
        );
    }

    /// The 3c.1.0 core-dumping row (https://github.com/rrnewton/hermit/issues/4571):
    /// a core-dumping signal is default-fatal too.
    #[test]
    fn core_dumping_signals_are_default_fatal() {
        for signal in [libc::SIGQUIT, libc::SIGABRT, libc::SIGBUS, libc::SIGFPE] {
            let s = state(0, bit(signal), 0, 0);
            assert_eq!(wait_end(&s, 0, 0), Some(WaitSignalEnd::Fatal(signal)));
        }
    }

    #[test]
    fn the_shared_queue_counts() {
        let s = state(0, bit(libc::SIGHUP), 0, 0);
        assert_eq!(wait_end(&s, 0, 0), Some(WaitSignalEnd::Fatal(libc::SIGHUP)));
    }

    #[test]
    fn a_fatal_signal_behind_a_caught_one_makes_the_end_fatal() {
        let s = state(
            bit(libc::SIGUSR1) | bit(libc::SIGTERM),
            0,
            0,
            bit(libc::SIGUSR1),
        );
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Fatal(libc::SIGTERM))
        );
    }

    /// `complete_signal`'s group exit at send (classify, rule 1): a deliverable
    /// default-fatal signal that does not dump core ends the wait as `Fatal`
    /// whatever Linux would dequeue first. Mutation "the first dequeued
    /// decides every end" (rule 1 removed): each row becomes `Caught` or names
    /// the core-dumping signal.
    #[test]
    fn a_non_core_dumping_fatal_signal_makes_the_end_fatal_whatever_is_dequeued_first() {
        let usr1 = bit(libc::SIGUSR1);
        for (name, thread, shared, caught, end) in [
            (
                "a private caught signal before a shared SIGTERM",
                usr1,
                bit(libc::SIGTERM),
                usr1,
                libc::SIGTERM,
            ),
            (
                "a lower-numbered private caught signal before a private SIGTERM",
                usr1 | bit(libc::SIGTERM),
                0,
                usr1,
                libc::SIGTERM,
            ),
            (
                "a synchronous core-dumping signal before a SIGTERM",
                bit(libc::SIGSEGV) | bit(libc::SIGTERM),
                0,
                0,
                libc::SIGTERM,
            ),
        ] {
            let s = state(thread, shared, 0, caught);
            assert_eq!(
                wait_end(&s, 0, 0),
                Some(WaitSignalEnd::Fatal(end)),
                "{name}"
            );
        }
    }

    /// A caught action for the walk's lookups: `mask`, and `SA_NODEFER` or not.
    fn action(mask: u64, nodefer: bool) -> CaughtAction {
        CaughtAction {
            mask,
            flags: if nodefer { libc::SA_NODEFER as u64 } else { 0 },
        }
    }

    /// The walk a `BehindCaught` end carries, for `state` and `mask`.
    fn walk(state: &KernelSignalState, mask: u64) -> DequeueWalk {
        match classify(state, deliverable(state, mask, 0), mask) {
            Some(WaitSignalEnd::BehindCaught(walk)) => walk,
            other => panic!("not a walk: {other:?}"),
        }
    }

    /// The end of a wait on `state` (guest mask 0), a `BehindCaught` one
    /// resolved with `action`.
    fn end_of(
        state: &KernelSignalState,
        action: impl FnMut(i32) -> Option<CaughtAction>,
    ) -> Result<WaitSignalEnd, WalkRefusal> {
        match classify(state, deliverable(state, 0, 0), 0) {
            Some(WaitSignalEnd::BehindCaught(walk)) => walk.resolve(action),
            Some(end) => Ok(end),
            None => panic!("nothing ends the wait"),
        }
    }

    /// Rule 2 with a caught signal first and a core-dumping one deliverable
    /// behind it (Claude review round 2): Linux sets up the caught handler and
    /// goes on delivering under its `sa_mask` (signal_delivered,
    /// kernel/signal.c:2941-2954), so the core-dumping signal ends the
    /// process unless that mask, or a later handler's, holds it. Rows: (a) the
    /// handler's mask blocks it, `Caught` (a private caught signal before a
    /// shared, lower-numbered SIGQUIT; both private, the caught one
    /// lower-numbered); (b) it does not, `Fatal`; (c) `SA_NODEFER` with one
    /// instance of the caught signal, decided by the mask alone (where it does
    /// matter, with `SA_RESETHAND`:
    /// `sa_resethand_and_a_second_instance_decide_the_end`); (d) two caught
    /// signals whose masks together block it,
    /// `Caught`. Mutations: "the handler's sa_mask ignored" ((a), (d) become
    /// `Fatal`), "only the first handler's mask" ((d) becomes `Fatal`), "any
    /// default-fatal signal makes the end fatal" ((a), (d)), "the lowest
    /// number across both queues" (the first (a) row becomes `Fatal(SIGQUIT)`
    /// at classification).
    #[test]
    fn a_caught_signal_dequeued_before_a_core_dumping_one_is_resolved_by_the_handlers_masks() {
        let (hup, usr1, usr2) = (bit(libc::SIGHUP), bit(libc::SIGUSR1), bit(libc::SIGUSR2));
        let (quit, xcpu) = (bit(libc::SIGQUIT), bit(libc::SIGXCPU));
        type Actions = &'static [(i32, u64, bool)];
        let rows: [(&str, u64, u64, u64, Actions, WaitSignalEnd); 7] = [
            (
                "(a) the private caught signal's mask blocks a shared SIGQUIT",
                usr2,
                quit,
                usr2,
                &[(libc::SIGUSR2, 1 << (libc::SIGQUIT - 1), false)],
                WaitSignalEnd::Caught(libc::SIGUSR2),
            ),
            (
                "(a) both private, the caught one's mask blocks SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 1 << (libc::SIGQUIT - 1), false)],
                WaitSignalEnd::Caught(libc::SIGHUP),
            ),
            (
                "(b) the caught signal's mask does not block SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 1 << (libc::SIGUSR1 - 1), false)],
                WaitSignalEnd::Fatal(libc::SIGQUIT),
            ),
            (
                "(c) SA_NODEFER, mask blocking SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 1 << (libc::SIGQUIT - 1), true)],
                WaitSignalEnd::Caught(libc::SIGHUP),
            ),
            (
                "(c) SA_NODEFER, mask not blocking SIGQUIT",
                hup | quit,
                0,
                hup,
                &[(libc::SIGHUP, 0, true)],
                WaitSignalEnd::Fatal(libc::SIGQUIT),
            ),
            (
                "(d) two caught signals whose masks together block SIGXCPU",
                hup | usr1 | xcpu,
                0,
                hup | usr1,
                &[
                    (libc::SIGHUP, 0, false),
                    (libc::SIGUSR1, 1 << (libc::SIGXCPU - 1), false),
                ],
                WaitSignalEnd::Caught(libc::SIGHUP),
            ),
            (
                "(d) neither mask blocks SIGXCPU",
                hup | usr1 | xcpu,
                0,
                hup | usr1,
                &[(libc::SIGHUP, 0, false), (libc::SIGUSR1, 0, false)],
                WaitSignalEnd::Fatal(libc::SIGXCPU),
            ),
        ];
        // Every row is checked before the test fails, so that a mutation's
        // failure names the rows it changes.
        let mut wrong = Vec::new();
        for (name, thread, shared, caught, actions, end) in rows {
            let s = state(thread, shared, 0, caught);
            let resolved = end_of(&s, |signal| {
                actions
                    .iter()
                    .find(|(known, _, _)| *known == signal)
                    .map(|(_, mask, nodefer)| action(*mask, *nodefer))
            });
            if resolved != Ok(end) {
                wrong.push(format!("{name}: {resolved:?}"));
            }
        }
        assert!(wrong.is_empty(), "rows: {wrong:#?}");
    }

    /// `SA_RESETHAND` and a second instance (both reviews, round 3): Linux
    /// resets an `SA_RESETHAND` action to `SIG_DFL` when it dequeues it
    /// (`get_signal`, kernel/signal.c:2821-2822 at v6.12), so a second
    /// instance of the same signal that its masks leave unblocked
    /// (`SA_NODEFER`, the legacy `signal()`'s flags, :4675) is taken at the
    /// default action. A standard signal pending on both queues is two
    /// instances; a real-time one may have any number. Rows: SIGUSR1 on both
    /// queues with `SA_RESETHAND|SA_NODEFER`, `Fatal(SIGUSR1)`; the same
    /// without `SA_NODEFER`, `Caught`; `SA_RESETHAND` alone with one instance,
    /// `Caught`; reset to a default-ignored SIGWINCH, discarded, `Caught`;
    /// reset to a default stop (SIGTSTP), `ResetToStop`; a real-time signal
    /// with `SA_RESETHAND|SA_NODEFER`, `RealtimeInstances`; the same without
    /// `SA_RESETHAND`, `Caught`; and `SA_RESETHAND|SA_NODEFER` with SIGUSR1's
    /// own mask holding it, `Caught`. Mutations: "SA_NODEFER ignored",
    /// "SA_RESETHAND ignored", "the second queue's instance ignored" and "the
    /// real-time refusal removed" each change a row.
    #[test]
    fn sa_resethand_and_a_second_instance_decide_the_end() {
        let (usr1, winch, tstp) = (bit(libc::SIGUSR1), bit(libc::SIGWINCH), bit(libc::SIGTSTP));
        let rt = 40;
        let rtbit = bit(rt);
        let resethand = libc::SA_RESETHAND as u64;
        let nodefer = libc::SA_NODEFER as u64;
        // name, private, shared, caught, the caught signal's action
        // (mask, flags), the end
        type Row = (
            &'static str,
            u64,
            u64,
            u64,
            (u64, u64),
            Result<WaitSignalEnd, WalkRefusal>,
        );
        let rows: [Row; 8] = [
            (
                "SIGUSR1 on both queues, SA_RESETHAND|SA_NODEFER",
                usr1,
                usr1,
                usr1,
                (0, resethand | nodefer),
                Ok(WaitSignalEnd::Fatal(libc::SIGUSR1)),
            ),
            (
                "SIGUSR1 on both queues, SA_RESETHAND without SA_NODEFER",
                usr1,
                usr1,
                usr1,
                (0, resethand),
                Ok(WaitSignalEnd::Caught(libc::SIGUSR1)),
            ),
            (
                "SIGUSR1 on both queues, SA_RESETHAND|SA_NODEFER, its own mask holding it",
                usr1,
                usr1,
                usr1,
                (usr1, resethand | nodefer),
                Ok(WaitSignalEnd::Caught(libc::SIGUSR1)),
            ),
            (
                "SIGUSR1 once, SA_RESETHAND|SA_NODEFER",
                usr1,
                0,
                usr1,
                (0, resethand | nodefer),
                Ok(WaitSignalEnd::Caught(libc::SIGUSR1)),
            ),
            (
                "SIGWINCH on both queues, reset to its default, ignore",
                winch,
                winch,
                winch,
                (0, resethand | nodefer),
                Ok(WaitSignalEnd::Caught(libc::SIGWINCH)),
            ),
            (
                "SIGTSTP on both queues, reset to its default, a stop",
                tstp,
                tstp,
                tstp,
                (0, resethand | nodefer),
                Err(WalkRefusal::ResetToStop {
                    signal: libc::SIGTSTP,
                }),
            ),
            (
                "a real-time signal, SA_RESETHAND|SA_NODEFER",
                rtbit,
                0,
                rtbit,
                (0, resethand | nodefer),
                Err(WalkRefusal::RealtimeInstances { signal: rt }),
            ),
            (
                "a real-time signal, SA_NODEFER without SA_RESETHAND",
                rtbit,
                0,
                rtbit,
                (0, nodefer),
                Ok(WaitSignalEnd::Caught(rt)),
            ),
        ];
        let mut wrong = Vec::new();
        for (name, thread, shared, caught, (mask, flags), end) in rows {
            let s = state(thread, shared, 0, caught);
            let resolved = end_of(&s, |_| Some(CaughtAction { mask, flags }));
            if resolved != end {
                wrong.push(format!("{name}: {resolved:?}"));
            }
        }
        assert!(wrong.is_empty(), "rows: {wrong:#?}");
    }

    /// (e) An action the checkpoint cannot read while a default-fatal signal
    /// is still deliverable behind its caught signal is
    /// `WalkRefusal::ActionUnknown`, which
    /// the checkpoint fails closed on; one it no longer needs is not asked
    /// for (the first handler's mask already holds the default-fatal signal).
    /// Mutation "no fail-closed" (a missing action taken as an empty mask):
    /// the unknown row becomes `Fatal(SIGQUIT)`.
    #[test]
    fn an_unreadable_caught_action_with_a_fatal_signal_behind_it_is_unknown() {
        let (hup, usr1, quit) = (bit(libc::SIGHUP), bit(libc::SIGUSR1), bit(libc::SIGQUIT));
        let s = state(hup | quit, 0, 0, hup);
        assert_eq!(
            walk(&s, 0).resolve(|_| None),
            Err(WalkRefusal::ActionUnknown {
                caught: libc::SIGHUP,
                behind: libc::SIGQUIT,
            })
        );
        let s = state(hup | usr1 | quit, 0, 0, hup | usr1);
        let mut asked = Vec::new();
        let resolved = walk(&s, 0).resolve(|signal| {
            asked.push(signal);
            (signal == libc::SIGHUP).then_some(action(quit, false))
        });
        assert_eq!(resolved, Ok(WaitSignalEnd::Caught(libc::SIGHUP)));
        assert_eq!(asked, [libc::SIGHUP]);
    }

    /// Rule 2 with a core-dumping signal first: `get_signal` ends the process
    /// for it, so the end is `Fatal` even though a caught signal is also
    /// deliverable. Mutations: "the caught signal always wins over a
    /// core-dumping one" (each row becomes `Caught`) and "synchronous signals
    /// not first" (the SIGSEGV row becomes `Caught(SIGHUP)`).
    #[test]
    fn a_core_dumping_signal_dequeued_first_makes_the_end_fatal() {
        for (name, thread, shared, caught, end) in [
            (
                "both private, the core-dumping one lower-numbered",
                bit(libc::SIGQUIT) | bit(libc::SIGUSR1),
                0,
                bit(libc::SIGUSR1),
                libc::SIGQUIT,
            ),
            (
                "a synchronous core-dumping signal before a lower-numbered caught one",
                bit(libc::SIGSEGV) | bit(libc::SIGHUP),
                0,
                bit(libc::SIGHUP),
                libc::SIGSEGV,
            ),
            (
                "a private core-dumping signal before a shared caught one",
                bit(libc::SIGABRT),
                bit(libc::SIGHUP),
                bit(libc::SIGHUP),
                libc::SIGABRT,
            ),
        ] {
            let s = state(thread, shared, 0, caught);
            assert_eq!(
                wait_end(&s, 0, 0),
                Some(WaitSignalEnd::Fatal(end)),
                "{name}"
            );
        }
    }

    #[test]
    fn the_first_caught_signal_follows_linux_dequeue_order() {
        // The private queue first, then synchronous before lower-numbered.
        let caught = bit(libc::SIGUSR1) | bit(libc::SIGUSR2) | bit(libc::SIGHUP);
        let s = state(bit(libc::SIGUSR2), bit(libc::SIGHUP), 0, caught);
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Caught(libc::SIGUSR2))
        );
    }

    #[test]
    fn reserved_signals_are_left_out() {
        let s = state(bit(libc::SIGSYS), 0, 0, bit(libc::SIGSYS));
        assert_eq!(deliverable(&s, 0, bit(libc::SIGSYS)), 0);
        assert_eq!(deliverable(&s, 0, 0), bit(libc::SIGSYS));
    }

    #[test]
    fn sigkill_is_always_deliverable_when_pending() {
        let s = state(bit(libc::SIGKILL), 0, 0, 0);
        assert_eq!(
            wait_end(&s, 0, 0),
            Some(WaitSignalEnd::Fatal(libc::SIGKILL))
        );
    }

    /// D44's refusal and its vfork narrowing. Mutations: "vfork not
    /// narrowed" (the caught and core-dumping vfork rows), "the mask
    /// ignored" (the blocked rows).
    #[test]
    fn a_kernel_run_send_is_refused_only_for_a_signal_that_would_interrupt_the_call() {
        let usr1 = bit(libc::SIGUSR1);
        let term = bit(libc::SIGTERM);
        let dispositions = state(0, 0, bit(libc::SIGHUP), usr1);
        for (signal, mask, vfork, refused, name) in [
            (libc::SIGTERM, 0, false, true, "default-fatal"),
            (libc::SIGQUIT, 0, false, true, "core-dumping"),
            (libc::SIGUSR1, 0, false, true, "caught"),
            (libc::SIGTERM, term, false, false, "blocked"),
            (libc::SIGHUP, 0, false, false, "SIG_IGN"),
            (libc::SIGURG, 0, false, false, "default-ignored"),
            (libc::SIGTSTP, 0, false, false, "default stop"),
            (libc::SIGTERM, 0, true, true, "vfork: default-fatal"),
            (libc::SIGQUIT, 0, true, false, "vfork: core-dumping"),
            (libc::SIGUSR1, 0, true, false, "vfork: caught"),
            (libc::SIGTERM, term, true, false, "vfork: blocked"),
        ] {
            assert_eq!(
                kernel_run_send_refused(&dispositions, mask, signal, vfork, 0),
                refused,
                "{name}"
            );
        }
        assert!(!kernel_run_send_refused(
            &dispositions,
            0,
            libc::SIGTERM,
            false,
            term
        ));
    }

    #[test]
    fn restart_codes() {
        for errno in [
            reverie::Errno::ERESTARTSYS,
            reverie::Errno::ERESTARTNOINTR,
            reverie::Errno::ERESTARTNOHAND,
            reverie::Errno::ERESTART_RESTARTBLOCK,
        ] {
            assert!(is_restart_code(errno));
        }
        assert!(!is_restart_code(reverie::Errno::EINTR));
    }
}
