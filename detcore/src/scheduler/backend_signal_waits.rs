/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `rt_sigsuspend` waits on a backend that reports the guest's signal state
//! itself (SIGSHARE S2b; design `ai_docs/2026-10-29-kvm-sigshare-design.md`,
//! S2 and S3).
//!
//! Where guest threads are not host tasks (`provides_process_signal_control`,
//! KVM), the backend runs `rt_sigsuspend` itself, and its call never sleeps:
//! it returns `EINTR` with the temporary mask installed for the delivery when
//! a pending signal that the mask lets in ends the call, and declines to wait
//! otherwise (`ENOSYS`). So Detcore parks the thread between two runs of the
//! backend's call (`Detcore::handle_rt_sigsuspend`): a
//! `SleepUntil(INDEFINITE)` request tagged `RT_SIGSUSPEND_FYI` that carries
//! the temporary mask (`Resources::blocked_signal_mask`). A masked `ppoll`
//! needs no decision here: it polls with the backend's own zero-timeout
//! probes (`Detcore::backend_masked_ppoll_wait`), each of which runs in the
//! thread's own turn and ends the wait when a signal its mask lets in is
//! pending.
//!
//! This module decides when a parked `rt_sigsuspend` ends
//! ([`Scheduler::wake_backend_signal_waits`]).
//! It reads the waiter's signal state from the backend
//! (`backend_signal_state`) and wakes it when a pending signal is unblocked by
//! the temporary mask (Linux's `signal_pending` once `sigsuspend` has
//! installed the mask). A caught or default-acting one ends the wait. An
//! ignored one is one Linux discarded when it was sent (`sig_ignored`,
//! kernel/signal.c: the mask installed then lets it in), which the backend
//! keeps pending because the thread's own mask, not the temporary one, is
//! installed while it waits; the woken thread removes it in its own turn
//! (`settle_suspend_decline`) and parks again, before a later disposition
//! change could make it deliverable (the other half of that rule is
//! `Scheduler::suspend_holds_discarded`, which a call that stops ignoring a
//! signal asks first). The woken thread runs the backend's call again in its
//! own turn, and that call decides what the guest sees; if the signal is gone
//! by then, the call declines again and the thread parks again, so a wake that
//! finds nothing is never visible to the guest.
//!
//! A shared copy that a guest send chose one thread of the process for (its
//! claim or taker mark, `guest_send_admission`), whether that thread waits
//! here, runs, or polls, wakes no other waiter of that process, as Linux's
//! `complete_signal` wakes only the thread it chooses (`signal_wake_up`,
//! kernel/signal.c:963-1032 at v7.1.3): a sibling the same copy would end
//! the wait of stays parked, with no turn, no `COMMIT` record and no charge
//! of virtual time. A claim or mark speaks only for its own process's
//! shared queue, so it never keeps another process's waiter asleep. A copy
//! in a waiter's own queue, or one no send chose a thread for, wakes every
//! waiter it ends, as before.
//!
//! It runs at one point: the end of the step-2 drain (`step2_drain_prefix`),
//! after every barrier, which the controlled turn loop reaches only while no
//! guest thread holds the turn and no exit fence, child-exit reservation,
//! retirement or resume registration is outstanding. The backend's signal
//! sets then change only by committed events (S1's contract for
//! `ProcessSignalControl::signal_sets`): a guest thread's own turn, or a
//! Detcore publication under the scheduler mutex, such as a timer expiry,
//! whose waiter the next drain wakes. The one exception is a delivery under
//! an observed parked wait's permit, which the observed thread's own host
//! thread selects from the process's queues, so a process any of whose
//! threads holds such a permit is passed over until it settles. An ordinary
//! boundary permit does not count: its holder selected inside its own serial
//! grant, before it filed the request that let this drain run, as
//! `select_parked_alarm` reasons. Which waiters are scanned, in which
//! order (DetTid), and what each read answers are therefore functions of the
//! committed schedule. The decision is level-triggered, from the state alone:
//! a signal that became pending by any committed event is seen at the first
//! drain after it, whoever sent it.
//!
//! A child's exit `SIGCHLD` is no exception (SIGSHARE S4). The backend
//! publishes it under the scheduler mutex, inside the child-exit reservation
//! the exit grant installs, which is a control barrier: no drain runs between
//! the grant and the publication's completion. So a `SIGCHLD` pending at a
//! drain is pending by a committed event, whichever thread count the parent
//! has, and it ends the wait as any other signal does.

use reverie::syscalls::Errno;

use super::Scheduler;
use super::kernel_signal_bit;
use super::sleep_signal::SleeperStateUnreadable;
use crate::resources::Permission;
use crate::resources::RT_SIGSUSPEND_FYI;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::KernelSignalState;
use crate::syscalls::ignored_at_generation;
use crate::syscalls::reads_signal_state_from_backend;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::SigWrapper;

impl Scheduler {
    /// The temporary mask of `dettid`'s parked `rt_sigsuspend`, if its filed
    /// request is one this module decides: only on a backend that reports the
    /// guest's signal state (`reads_signal_state_from_backend`) and outside
    /// the in-guest delivery gate, a one-resource indefinite
    /// `RT_SIGSUSPEND_FYI` sleep that carries its temporary mask.
    pub(crate) fn backend_suspend_mask(&self, dettid: DetTid) -> Option<u64> {
        if !reads_signal_state_from_backend(&self.backend) || self.in_guest_signal_delivery {
            return None;
        }
        let resources = self.next_turns.get(&dettid)?.req.try_read()?.ok()?;
        if resources.resources.len() != 1 {
            return None;
        }
        let mask = resources.blocked_signal_mask?;
        match resources.resources.iter().next() {
            Some((ResourceID::SleepUntil(deadline), _))
                if deadline.is_indefinite() && resources.fyi.as_str() == RT_SIGSUSPEND_FYI =>
            {
                Some(mask)
            }
            _ => None,
        }
    }

    /// Called wherever a thread's filed request is replaced (a wake): if it
    /// was a parked `rt_sigsuspend` this module decides, the thread's
    /// temporary mask is kept (`backend_suspend_woken`) until its grant, so
    /// `suspend_holds_discarded` still sees the waiter while it is woken but
    /// not yet served, whatever woke it.
    pub(crate) fn note_backend_suspend_replaced(&mut self, dettid: DetTid) {
        if let Some(mask) = self.backend_suspend_mask(dettid) {
            self.backend_suspend_woken.insert(dettid, mask);
        }
    }

    /// The signals that end or settle a parked `rt_sigsuspend` under temporary
    /// mask `mask`, given the waiter's signal state as the backend reports it:
    /// pending for it, unblocked by `mask`, and caught or acting by default
    /// (`KernelSignalState::interrupting`), or ignored at generation
    /// (`ignored_at_generation`), which Linux discarded when they were sent
    /// and the woken thread removes in its own turn. Held: the signals a host-timed
    /// source armed by a guest can post (`host_timed_signals`). A child's exit
    /// `SIGCHLD` counts as any other signal (see the module documentation).
    pub(crate) fn backend_suspend_signals(
        state: &KernelSignalState,
        mask: u64,
        host_timed: u64,
    ) -> u64 {
        let let_in = state.pending
            & !host_timed
            & !mask
            & !kernel_signal_bit(reverie::PERF_EVENT_SIGNAL as i32);
        let discarded = let_in & ignored_at_generation(state.ignored, state.caught);
        (let_in & state.interrupting(mask)) | discarded
    }

    /// Whether an `rt_sigsuspend` of `process` whose temporary mask lets
    /// `signal` in has it pending, on its own queue or the process's, while
    /// the process ignores it: Linux discarded it when it was sent
    /// (`sig_ignored`, kernel/signal.c), and the waiter has not yet removed it
    /// in its own turn. The waiter is either parked (`backend_suspend_mask`)
    /// or woken by the drain and not yet granted its turn
    /// (`backend_suspend_woken`): its request is then the signal set, not the
    /// park, and the signal is still pending until that turn. A call that
    /// would stop ignoring `signal` (`Detcore::handle_rt_sigaction`) asks this
    /// first, and does not change the action while it is true, so the change
    /// never makes deliverable a signal Linux no longer holds. Waiters are
    /// read from their committed state, in DetTid order; a gone or unreadable
    /// one holds nothing (the scan names the latter).
    pub(crate) fn suspend_holds_discarded(&self, process: DetPid, signal: i32) -> bool {
        if !reads_signal_state_from_backend(&self.backend)
            || self.in_guest_signal_delivery
            || !(1..=64).contains(&signal)
        {
            return false;
        }
        let bit = kernel_signal_bit(signal);
        self.next_turns.keys().any(|&dettid| {
            self.backend_suspend_mask(dettid)
                .or_else(|| self.backend_suspend_woken.get(&dettid).copied())
                .is_some_and(|mask| mask & bit == 0)
                && self.registered_process(dettid) == Some(process)
                && self.backend_waiter_is_current(process, dettid)
                && self.backend_wait_state(dettid).is_ok_and(|state| {
                    state.pending & bit != 0
                        && ignored_at_generation(state.ignored, state.caught) & bit != 0
                })
        })
    }

    /// `dettid`'s signal state for a wait decision, through the backend.
    fn backend_wait_state(
        &self,
        dettid: DetTid,
    ) -> Result<KernelSignalState, SleeperStateUnreadable> {
        match self.backend_signal_state(dettid) {
            Some(Ok(state)) => Ok(state),
            Some(Err(Errno::ESRCH)) => Err(SleeperStateUnreadable::Gone),
            Some(Err(errno)) => Err(SleeperStateUnreadable::Unreadable(errno)),
            None => Err(SleeperStateUnreadable::Unreadable(Errno::ENOSYS)),
        }
    }

    /// Whether some thread of `process` holds an observed parked wait's
    /// delivery permit (`site` set): the observed thread's own host thread may
    /// take a signal of the process for it, which changes the process's
    /// pending set and the thread's mask without a turn of the scheduler's,
    /// so the process's waits are not decided until it settles. An ordinary
    /// boundary permit (`site` None) does not count, for the reason
    /// `select_parked_alarm` gives: its holder selected inside its own serial
    /// grant, before it filed the request that let the drain run, and the
    /// delivery it completes runs in its own granted turn. Counting it held a
    /// suspended thread forever behind a sibling that signals itself in a
    /// loop (`run_kvm_signal_permits_are_per_thread`, `alarm-sigsuspend`).
    fn process_holds_observation_permit(&self, process: DetPid) -> bool {
        self.parked.permits.iter().any(|(holder, permit)| {
            permit.site.is_some() && self.registered_process(*holder) == Some(process)
        })
    }

    /// Whether `dettid` is still the live task lifetime its request was
    /// filed by, as `select_parked_alarm` requires before acting for it.
    fn backend_waiter_is_current(&self, process: DetPid, dettid: DetTid) -> bool {
        if self.thread_is_logically_killed(dettid) {
            return false;
        }
        self.real_timers
            .task_identity(process, dettid)
            .is_some_and(|(mm, _)| self.rpc_incarnation_matches(dettid, mm))
    }

    /// The shared copies a guest send to `thread`'s process chose it to take:
    /// its claims (`guest_send_claims`) and its taker mark
    /// (`process_signal_takers`).
    fn shared_copies_chosen_for(&self, thread: DetTid) -> u64 {
        self.guest_send_claims
            .get(&thread)
            .map_or(0, |claim| claim.shared)
            | self
                .process_signal_takers
                .get(&thread)
                .copied()
                .unwrap_or(0)
    }

    /// The shared copies a guest send chose a thread of `process` other than
    /// `thread` to take, whatever that thread is doing: claims and taker
    /// marks speak for their own process's shared queue only.
    fn shared_copies_chosen_for_another(&self, process: DetPid, thread: DetTid) -> u64 {
        self.guest_send_claims
            .keys()
            .chain(self.process_signal_takers.keys())
            .filter(|other| **other != thread && self.registered_process(**other) == Some(process))
            .fold(0, |bits, other| {
                bits | self.shared_copies_chosen_for(*other)
            })
    }

    /// Ends every parked `rt_sigsuspend` this module decides
    /// (`backend_suspend_mask`) that a pending signal now ends
    /// (`backend_suspend_signals`). Called only at
    /// the end of the step-2 drain (see the module documentation); waiters are
    /// visited in DetTid order. A woken waiter's request becomes the signal
    /// set (`WaitidSignals`) at the front of its band, as a woken sleep's does
    /// (`drain_sleep_signals`), and its own turn runs the backend's call again.
    /// A thread that is gone is left to its retirement; a state that cannot be
    /// read otherwise ends the run with a diagnostic, never a guessed wake.
    pub(crate) fn wake_backend_signal_waits(&mut self) {
        if !reads_signal_state_from_backend(&self.backend) || self.in_guest_signal_delivery {
            return;
        }
        let waiters: Vec<(DetTid, u64)> = self
            .next_turns
            .keys()
            .filter_map(|&tid| self.backend_suspend_mask(tid).map(|mask| (tid, mask)))
            .collect();
        // Each waiter a signal ends the wait of: its thread, its process,
        // its temporary mask, the signals, and those of them only its
        // process's shared queue holds.
        let mut ending: Vec<(DetTid, DetPid, u64, u64, u64)> = Vec::new();
        for (dettid, mask) in waiters {
            let Some(owner) = self.registered_process(dettid) else {
                continue;
            };
            if self.process_holds_observation_permit(owner)
                || !self.backend_waiter_is_current(owner, dettid)
            {
                continue;
            }
            let state = match self.backend_wait_state(dettid) {
                Ok(state) => state,
                Err(SleeperStateUnreadable::Gone) => {
                    tracing::debug!(
                        "[dtid {}] is gone; its rt_sigsuspend is left to its retirement.",
                        dettid
                    );
                    continue;
                }
                Err(SleeperStateUnreadable::Unreadable(errno)) => {
                    self.terminal_deadlock.get_or_insert_with(|| {
                        format!(
                            "HERMIT_DEADLOCK: cannot read the signal state of dettid {} ({}) to \
                             decide whether a signal ends its rt_sigsuspend",
                            dettid, errno
                        )
                    });
                    return;
                }
            };
            let signals =
                Self::backend_suspend_signals(&state, mask, self.host_timed_signals(dettid));
            if signals == 0 {
                continue;
            }
            ending.push((
                dettid,
                owner,
                mask,
                signals,
                signals & state.shared_pending & !state.thread_pending,
            ));
        }
        for &(dettid, owner, mask, signals, shared_only) in &ending {
            // Linux's `complete_signal` wakes the one thread it chooses for a
            // process-directed signal (`signal_wake_up`). A waiter whose
            // signals are all shared copies that a guest send chose another
            // thread of its own process for (its claim or taker mark), and
            // not it, is left asleep, whether that thread waits here or not:
            // that thread takes them. A copy in its own queue, or one chosen
            // for no thread of its process, still wakes it. The rule is
            // judged again from the state at every drain, so a copy its chosen
            // thread does not take (it blocks the signal, or exits) reaches
            // this waiter once the claim and the mark are gone. Until then the
            // claims audit refuses that case by name once it can read the
            // chosen thread's state (`ClaimReason::Retarget`,
            // `Scheduler::audit_guest_send_claims`); an unreadable claim it
            // defers to its next pass.
            let for_another = self.shared_copies_chosen_for_another(owner, dettid)
                & shared_only
                & !self.shared_copies_chosen_for(dettid);
            if signals & !for_another == 0 {
                tracing::info!(
                    "[dtid {}] stays in its rt_sigsuspend: signals {:#x} it would end for are shared \
                     copies a guest send chose another thread of its process for.",
                    dettid,
                    signals
                );
                continue;
            }
            let signals: Vec<SigWrapper> = (1..=64)
                .filter(|signal| signals & kernel_signal_bit(*signal) != 0)
                .map(SigWrapper)
                .collect();
            tracing::info!(
                "[dtid {}] signals {:?} end or settle its rt_sigsuspend under mask {:#x}, as the backend \
                 reports its signal state.",
                dettid,
                signals,
                mask
            );
            if self.run_queue.contains_tid(dettid) {
                let removed = self.run_queue.remove_tid(dettid);
                debug_assert!(
                    removed,
                    "run_queue.contains_tid disagreed with remove_tid for {dettid}"
                );
            }
            let mut resources = Resources::new(dettid);
            resources.insert(ResourceID::WaitidSignals(signals), Permission::W);
            self.force_unblock_thread_at(dettid, resources, true);
        }
    }
}
