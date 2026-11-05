/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The child-exit `SIGCHLD` of a controlled exit, decided by the scheduler
//! (SIGSHARE S4, single-threaded parents; design
//! `ai_docs/2026-10-29-kvm-sigshare-design.md`, sections 7.3, 8.4 and 10).
//!
//! On a backend that owns signal delivery (`provides_process_signal_control`,
//! KVM) every exit is `Controlled`: the backend publishes the child's exit to
//! its parent itself, inside the child-exit reservation the exit grant
//! installs (`signal_control.rs`), and before this module Detcore took no
//! decision of its own. ptrace's parent instead gets the scheduler's
//! decision: a classification at the child's `Exit` grant
//! (`classify_child_exit_notification`), a `ChildExit` timer one nanosecond
//! after it, and at the timer's pop the same send path as any scheduler
//! signal (`fire_child_exit_alarm` -> `fire_alarm_at` -> `send_guest_signal`),
//! which logs the decision and wakes a parked parent.
//!
//! Here a controlled exit whose parent has one live thread takes that same
//! path. Only the physical act differs, the design's "sink": the backend's
//! publication is the physical send, and it has already happened when the
//! timer pops, because the reservation is a control barrier and the timer
//! pops only at a maintenance step, which the controlled turn loop runs only
//! with the barrier down (`do_a_turn_blocking`). So `send_guest_signal` sends
//! nothing on such a backend; it takes the publication's effect instead
//! ([`Scheduler::child_exit_sink`]):
//! - `Queued`: the copy is pending. The parent is woken as on ptrace when
//!   the copy interrupts its wait (unblocked, and caught or default-fatal);
//!   an unblocked ignored copy is logged as ptrace's discard, and a blocked
//!   one wakes nothing ([`Scheduler::queued_sigchld_fate`]);
//! - `Coalesced`: Linux does not queue a standard signal already pending
//!   (`legacy_queue`), so no thread is woken, as `note_group_send` decides on
//!   ptrace;
//! - `SuppressedExplicitIgnore` or `DiscardedByDispositionChange`: Linux sent
//!   nothing, logged as the discarded-at-send record does on ptrace, and no
//!   thread is woken.
//!
//! The decision and the effect are checked against each other when the
//! publication completes ([`Scheduler::check_child_exit_publication`]). A
//! disagreement Linux cannot produce ends the run by name
//! (`sigchld-decision-mismatch`), never a silent difference. One disagreement
//! is expected and allowed: a default-ignored, unblocked `SIGCHLD`, which
//! Linux discards at generation (`Discard`), is queued by the backend
//! (design G8; S0 correction 2).
//!
//! A parent with more than one live thread keeps the backend's decision, as
//! before: choosing which of its threads takes the signal needs the scheduler
//! to model signal targets on this backend (SIGSHARE S3b).

use std::collections::BTreeMap;

use reverie::ChildExitPublication;
use reverie::ChildExitPublicationEffect;
use tracing::info;

use super::ChildExitNotification;
use super::Scheduler;
use super::signal_control::ExitReserveMode;
use crate::syscalls::reads_signal_state_from_backend;
use crate::types::DetPid;
use crate::types::DetTid;

/// The scheduler's child-exit decisions for controlled exits, from the
/// child's `Exit` grant to its `ChildExit` timer's pop.
///
/// Every entry is made and removed at a committed point under the scheduler
/// mutex: the grant, the publication's completion (inside the reservation's
/// control barrier, before any later turn), and the timer's pop.
#[derive(Debug, Default)]
pub(crate) struct ControlledChildExits {
    /// The decision at each child's `Exit` grant, until its publication
    /// completes.
    decisions: BTreeMap<DetPid, ChildExitNotification>,
    /// The publication effect of each child whose `ChildExit` timer has yet to
    /// pop.
    effects: BTreeMap<DetPid, ChildExitPublicationEffect>,
    /// The child whose `ChildExit` timer is sending its `SIGCHLD` right now
    /// (`send_guest_signal`).
    sending: Option<DetPid>,
    /// A decision and an effect that disagree: the run is refused.
    refusal: Option<String>,
}

/// What a queued child-exit `SIGCHLD` does to its receiver's parked wait
/// (`Scheduler::queued_sigchld_fate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueuedSigchld {
    /// Unblocked and caught (or its state unreadable): it ends the wait.
    Interrupts,
    /// Unblocked and ignored, explicitly or by default: Linux discards it at
    /// generation.
    DiscardedAtGeneration,
    /// Blocked: it stays pending, and the wait goes on.
    StaysPending,
}

/// Whether a child-exit publication's effect (and its waitability) agrees
/// with the scheduler's decision at the exit grant. `None` if it does,
/// otherwise why not.
pub(crate) fn child_exit_effect_mismatch(
    decision: ChildExitNotification,
    effect: ChildExitPublicationEffect,
    waitable: bool,
) -> Option<&'static str> {
    use ChildExitNotification as N;
    use ChildExitPublicationEffect as E;
    match (decision, effect) {
        // Linux sends nothing under an explicit SIG_IGN, and reaps the child
        // itself (`do_notify_parent`): no status remains to wait for.
        (N::None, E::SuppressedExplicitIgnore) if !waitable => None,
        (N::None, E::SuppressedExplicitIgnore) => {
            Some("the parent ignores SIGCHLD, but the child stayed waitable")
        }
        (N::None, _) => Some("the parent ignores SIGCHLD, but the backend did not suppress it"),
        // Linux discards a default-ignored, unblocked SIGCHLD at generation;
        // the backend queues it (design G8), which no wake follows.
        (N::Discard, E::Queued | E::Coalesced) => None,
        (N::Discard, _) => {
            Some("SIGCHLD is default-ignored, but the backend reports an explicit ignore")
        }
        (N::Deliver, E::Queued | E::Coalesced) => None,
        (N::Deliver, _) => Some("SIGCHLD is delivered, but the backend did not queue it"),
        (N::Undecided, _) => None,
    }
}

#[cfg(test)]
impl ControlledChildExits {
    /// The run's refusal, if one was made, without taking it.
    pub(super) fn refusal_for_test(&self) -> Option<&str> {
        self.refusal.as_deref()
    }
}

impl Scheduler {
    /// The decision recorded at `child`'s controlled exit grant, until its
    /// publication completes.
    #[cfg(test)]
    pub(super) fn controlled_child_exits_decision(
        &self,
        child: DetPid,
    ) -> Option<ChildExitNotification> {
        self.controlled_child_exits.decisions.get(&child).copied()
    }

    /// Whether the scheduler decides the child-exit `SIGCHLD` of a controlled
    /// exit (`reserve_mode`) of `child` to `parent`: on a backend whose signal
    /// state Detcore reads from the backend, outside the in-guest delivery
    /// gate, for a child whose exit reserved its publication (the parent is
    /// live), when the parent has exactly one live thread.
    pub(super) fn decides_controlled_child_exit(
        &self,
        reserve_mode: ExitReserveMode,
        child: DetPid,
        parent: DetPid,
    ) -> bool {
        reserve_mode == ExitReserveMode::Controlled
            && reads_signal_state_from_backend(&self.backend)
            && !self.in_guest_signal_delivery
            && self.child_exit_reserved(child)
            && self.process_signal_targets(parent).len() == 1
    }

    /// Record the scheduler's decision at the controlled `Exit` grant of
    /// `child`.
    pub(super) fn record_controlled_child_exit(
        &mut self,
        child: DetPid,
        decision: ChildExitNotification,
    ) {
        self.controlled_child_exits
            .decisions
            .insert(child, decision);
    }

    /// Check a committed child-exit publication against the scheduler's
    /// decision at the exit grant, if it took one, and keep its effect for the
    /// `ChildExit` timer. Called when the publication completes, under the
    /// scheduler mutex, while the reservation's control barrier still holds
    /// every turn and every maintenance step.
    pub(super) fn check_child_exit_publication(&mut self, receipt: &ChildExitPublication) {
        let child = DetPid::from_raw(receipt.completion.child.tgid.as_raw());
        let Some(decision) = self.controlled_child_exits.decisions.remove(&child) else {
            return;
        };
        if let Some(why) =
            child_exit_effect_mismatch(decision, receipt.effect, receipt.completion.waitable)
        {
            let reason = format!(
                "HERMIT_REFUSAL sigchld-decision-mismatch: the exit of child {} decided {:?} \
                 at its grant, and the backend published {:?} (waitable: {}): {}",
                child, decision, receipt.effect, receipt.completion.waitable, why
            );
            tracing::error!("{}", reason);
            self.controlled_child_exits.refusal.get_or_insert(reason);
            return;
        }
        if matches!(
            decision,
            ChildExitNotification::Deliver | ChildExitNotification::Undecided
        ) {
            self.controlled_child_exits
                .effects
                .insert(child, receipt.effect);
        }
    }

    /// Take the run's `sigchld-decision-mismatch` refusal, if one was made.
    pub(crate) fn take_sigchld_decision_refusal(&mut self) -> Option<String> {
        self.controlled_child_exits.refusal.take()
    }

    /// Run `fire` with `child` named as the child whose `ChildExit` timer is
    /// sending (`child_exit_sink`), where a signal control is installed. The
    /// timer's effect is consumed by its pop either way: a timer whose parent
    /// is gone sends nothing, and its effect must not stand for a later child
    /// that reuses the PID.
    pub(super) fn sending_controlled_child_exit<R>(
        &mut self,
        child: DetPid,
        fire: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if !self.signal_control_installed() {
            return fire(self);
        }
        let previous = self.controlled_child_exits.sending.replace(child);
        let result = fire(self);
        self.controlled_child_exits.sending = previous;
        self.controlled_child_exits.effects.remove(&child);
        result
    }

    /// What the `SIGCHLD` a child-exit publication queued for `dettid` does to
    /// the wait `dettid` is parked in. `Queued` says only that the backend
    /// queued a copy, not that it interrupts anything. Linux wakes a waiting
    /// task for a signal only when the signal is unblocked and acted on
    /// (caught, or default-fatal; `signal_wake_up` from `complete_signal`,
    /// past `sig_ignored` and `wants_signal`). A blocked copy stays pending
    /// and wakes nothing. An unblocked one that is ignored, explicitly or by
    /// default, is discarded at generation (`sig_ignored`), which the backend
    /// does not model (design G8, S0 correction 2): ptrace's send records that
    /// discard, so this does too.
    ///
    /// The receiver's sets are the backend's (`backend_signal_state`, S2a),
    /// under the mask the guest published while Detcore holds a private one
    /// (`under_published_mask`), filtered as the drain filters a restartable
    /// poll (`poll_wake_signals`). They are read at the timer's pop, a
    /// committed point where no guest thread holds the turn, so the answer is
    /// a function of the schedule. An unreadable state wakes the thread, as
    /// before this filter.
    ///
    /// Without this, a parked `nanosleep` (a wait the grant leaves Undecided)
    /// was woken for a blocked or default-ignored `SIGCHLD` and answered EINTR
    /// on KVM, where the sleep cannot judge the wake itself (`signal_sleep`;
    /// the revastra2 review of https://github.com/rrnewton/hermit/pull/5192,
    /// F1).
    pub(super) fn queued_sigchld_fate(&self, dettid: DetTid) -> QueuedSigchld {
        let bit = super::kernel_signal_bit(libc::SIGCHLD);
        match self.backend_signal_state(dettid) {
            Some(Ok(state)) => {
                let state = self.under_published_mask(dettid, state);
                if super::poll_wake_signals(&state, self.models_signal_targets) & bit != 0 {
                    QueuedSigchld::Interrupts
                } else if state.discarded_at_generation() & bit != 0 {
                    QueuedSigchld::DiscardedAtGeneration
                } else {
                    QueuedSigchld::StaysPending
                }
            }
            Some(Err(_)) | None => QueuedSigchld::Interrupts,
        }
    }

    /// Commit a child-exit `SIGCHLD` Linux discarded: ptrace's
    /// discarded-at-send record, no delivery, and a wake that commits no
    /// `InboundSignal` (`wake_signaled_guest_as`).
    fn discard_child_exit_sigchld(
        &mut self,
        dettid: DetTid,
        signal: nix::sys::signal::Signal,
        named: DetTid,
    ) {
        info!(
            target: "detcore::scheduler",
            "[dtid {}] signal {} is one Linux discards as sent to {}; committing no wake.",
            dettid, signal, named
        );
        // No delivery follows to consume the process's mark
        // (`dispatch_timed_signal_at`).
        let process = self.sigchld_process(dettid);
        self.blocked.sigchld_ready.remove(&process);
        if self.send_wakes(dettid) {
            self.wake_signaled_guest_as(dettid, signal, true);
        }
    }

    /// `send_guest_signal`'s physical step where a signal control is
    /// installed: the backend's child-exit publication already made it, so
    /// the scheduler acts on its effect. A send of anything else, or of a
    /// `SIGCHLD` no publication made, would reach a host process that is not
    /// the guest's: it is refused by name instead.
    ///
    /// `dettid` is the receiving thread and `named` the thread Linux names
    /// (`fire_child_exit_alarm`).
    pub(super) fn child_exit_sink(
        &mut self,
        dettid: DetTid,
        signal: nix::sys::signal::Signal,
        named: DetTid,
    ) {
        let effect = self
            .controlled_child_exits
            .sending
            .filter(|_| signal == nix::sys::signal::Signal::SIGCHLD)
            .and_then(|child| self.controlled_child_exits.effects.remove(&child));
        let Some(effect) = effect else {
            let reason = format!(
                "HERMIT_REFUSAL sigchld-decision-mismatch: the scheduler would send signal {} \
                 to dettid {} on a backend that owns signal delivery, with no publication \
                 standing for it",
                signal, dettid
            );
            tracing::error!("{}", reason);
            self.controlled_child_exits.refusal.get_or_insert(reason);
            self.guest_send_landed = Some(false);
            return;
        };
        self.note_scheduler_signal_sent(dettid, signal as libc::c_int);
        self.guest_send_landed = Some(true);
        match effect {
            ChildExitPublicationEffect::Queued => match self.queued_sigchld_fate(dettid) {
                QueuedSigchld::Interrupts => {
                    if self.send_wakes(dettid) {
                        self.wake_signaled_guest_as(dettid, signal, false);
                    }
                }
                QueuedSigchld::DiscardedAtGeneration => {
                    self.discard_child_exit_sigchld(dettid, signal, named);
                }
                QueuedSigchld::StaysPending => {
                    info!(
                        target: "detcore::scheduler",
                        "[dtid {}] SIGCHLD blocked by its receiver stays pending; waking no thread.",
                        dettid
                    );
                }
            },
            ChildExitPublicationEffect::Coalesced => {
                info!(
                    target: "detcore::scheduler",
                    "[dtid {}] SIGCHLD coalesced into the one pending in its process's shared queue; waking no thread.",
                    dettid
                );
            }
            ChildExitPublicationEffect::SuppressedExplicitIgnore
            | ChildExitPublicationEffect::DiscardedByDispositionChange => {
                self.discard_child_exit_sigchld(dettid, signal, named);
            }
        }
    }
}
