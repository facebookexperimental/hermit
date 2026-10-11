/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Retiring a SIGKILLed process at the kill turn on ptrace
//! (<https://github.com/rrnewton/hermit/issues/3994>).
//!
//! On a backend whose only report of a thread's death is its own
//! deregistration, a process killed by another guest process used to leave the
//! scheduler whenever that report arrived, at a moment the host chose. Here the
//! scheduler's view changes at one committed point instead, the sending turn:
//!
//! 1. **Reserved.** Before the physical send, the sender reserves the victims:
//!    every live thread of the target process except its own, with the
//!    address-space identity each registered with (`Scheduler::thread_mm`).
//!    A victim's consuming receipt (its deregistration) that arrives now is
//!    accounted but held: it retires nothing ([`Scheduler::route_sigkill_receipt`]).
//! 2. **Committed.** After a successful send, the scheduler retires each victim
//!    in tid order (`logically_kill_thread`). A victim with a filed request is
//!    never answered or granted again: the kernel ends the tracee, and the
//!    backend's exit path ends its task without the tool injecting anything. A
//!    failed send unwinds the reservation: a held receipt then takes the
//!    ordinary path, as if no kill had been prepared.
//! 3. **Physical exit accounted.** Each committed victim's first consuming
//!    receipt is accounted as usual; one held since state 1 already was.
//! 4. **Fence released.** Turn selection waits until every committed victim is
//!    in state 3 ([`Scheduler::sigkill_fence_holds`]), so the physical death is
//!    settled before the next guest instruction; host timing decides only how
//!    long the pause lasts.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use tracing::info;

use super::Scheduler;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::FutexID;
use crate::types::MmId;

/// A victim of a reserved send: a thread, its registered process, and the
/// address-space identity it registered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SigkillVictim {
    pub(crate) tid: DetTid,
    pub(crate) process: DetPid,
    pub(crate) mm: MmId,
}

#[derive(Debug)]
struct Reservation {
    victims: Vec<SigkillVictim>,
    /// The target's real uid, read by the sender before the send, for the
    /// parent's `CLD_KILLED` siginfo.
    uid: u32,
    /// Whether the victims include the sender's own process: such a send is
    /// committed at the sender's `Exit` grant, before it is sent.
    includes_sender: bool,
}

/// A reserved send: its token, and whether the victims include the sender's
/// own process, whose send cannot return.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub struct SigkillReservation {
    /// The reservation's token, for the commit.
    pub token: u64,
    /// Whether the victims include the sender's own process.
    pub includes_sender: bool,
}

/// Why a send is not reserved, so it keeps the ordinary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SigkillRefusal {
    /// The backend reports deaths some other way, or kills through its own
    /// path (`note_process_sigkill`).
    Backend,
    /// The target is not a live registered process.
    NoTarget,
    /// A victim has no recorded address-space identity.
    UnknownAddressSpace(DetTid),
    /// A victim is already reserved by an open send.
    AlreadyReserved(DetTid),
    /// In record or replay, a victim has a background operation in flight:
    /// its result may never be recorded, so the send keeps the ordinary path.
    BackgroundOperation(DetTid),
    /// A victim process is a PID namespace's init, which Linux protects from
    /// SIGKILL sent inside its namespace: the kill may be discarded.
    NamespaceInit(DetPid),
}

/// A SIGKILL fence, or a sender's `kill`, waited past
/// `Scheduler::SIGKILL_FENCE_VALVE` for victims' deaths or exit publications
/// that never arrived. Continuing would let host timing decide the schedule,
/// so the run is refused (<https://github.com/rrnewton/hermit/issues/3994>).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigkillFenceRefusal {
    /// Committed victims whose physical exit was not accounted, in tid order.
    pub unaccounted: Vec<DetTid>,
    /// Processes whose exit publication to the parent had not arrived.
    pub unpublished: Vec<DetPid>,
    /// How long, in host time, the wait had lasted.
    pub waited: std::time::Duration,
}

impl std::fmt::Display for SigkillFenceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hermit refused to continue the run: a SIGKILL's victims waited {:?} for \
             backend events that never arrived: unaccounted threads {:?}, unpublished \
             exits {:?}",
            self.waited,
            self.unaccounted
                .iter()
                .map(|t| t.as_raw())
                .collect::<Vec<_>>(),
            self.unpublished
                .iter()
                .map(|p| p.as_raw())
                .collect::<Vec<_>>(),
        )
    }
}

/// Whether a backend lets the scheduler retire a SIGKILL's victims at the
/// kill turn: ptrace, where the scheduler's record decides waits and each
/// victim's death reaches it as a deregistration. Backends that cancel killed
/// threads' RPCs, need thread-directed process signals, or report process
/// exits on their own keep their existing paths.
pub(crate) fn retires_sigkill_at_the_kill_turn(backend: &reverie::BackendCapabilities) -> bool {
    !(backend.needs_killed_thread_rpc_cancellation
        || backend.requires_thread_directed_process_signals
        || backend.process_exits_complete_asynchronously
        || backend.reports_physical_process_exits)
}

/// What a consuming receipt (a deregistration) of `tid` means now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SigkillReceipt {
    /// Not a victim: the ordinary path retires it.
    Ordinary,
    /// A reserved victim, before the commit: account it, retire nothing.
    Held,
    /// A committed victim: already retired; this accounts its physical exit.
    Committed,
}

#[derive(Debug, Default)]
pub(crate) struct SigkillRetirement {
    next_token: u64,
    reservations: BTreeMap<u64, Reservation>,
    /// Reserved victims, before their send commits or unwinds.
    reserved: BTreeMap<DetTid, u64>,
    /// Reserved victims whose consuming receipt arrived before the commit.
    held_receipts: BTreeSet<DetTid>,
    /// Committed victims whose physical exit is not accounted yet.
    unaccounted: BTreeSet<DetTid>,
    /// Owner-death wake batches from victims' cleanup, keyed by (victim, the
    /// batch's position among that victim's batches). They run when the
    /// victims' cohort is complete, in key order
    /// ([`Scheduler::run_retained_sigkill_wakes`]), never at receipt.
    retained_wakes: BTreeMap<(DetTid, u64), Vec<(DetTid, FutexID)>>,
    /// The next batch position of each victim with retained batches.
    wake_batches: BTreeMap<DetTid, u64>,
    /// Victims whose first consuming receipt was counted (held or committed):
    /// a later deregistration of the same thread changes nothing. Forgotten
    /// when a new thread registers on the tid (`forget_committed_exit`).
    accounted: BTreeSet<DetTid>,
    /// Victim processes whose exit publication the commit holds for: the
    /// fence waits for these as well, under the same valve.
    unpublished: BTreeSet<DetPid>,
    /// The processes of the send being committed, while the commit retires
    /// them one by one. A subreaper among them adopts none of the others'
    /// orphans ([`Scheduler::is_dying_sigkill_victim`]).
    dying: BTreeSet<DetPid>,
}

impl Scheduler {
    /// Reserve the victims of a SIGKILL that `sender` is about to send to
    /// `target`, before the physical send. Returns the reservation's token.
    pub(crate) fn reserve_sigkill(
        &mut self,
        sender: DetTid,
        target: DetPid,
        uid: u32,
    ) -> Result<SigkillReservation, SigkillRefusal> {
        self.reserve_sigkill_processes(sender, &[target], uid)
    }

    /// Reserve the victims of a `kill(-pgid, SIGKILL)` that `sender` is about
    /// to send: every live process whose recorded process group is `pgid`.
    /// Every guest process has the same credentials (the credential setters
    /// are deterministic no-ops, and ptrace sets `PR_SET_NO_NEW_PRIVS`), so
    /// Linux signals every member, and so all share `uid`.
    pub(crate) fn reserve_sigkill_group(
        &mut self,
        sender: DetTid,
        pgid: DetPid,
        uid: u32,
    ) -> Result<SigkillReservation, SigkillRefusal> {
        let mut members: Vec<DetPid> = self
            .thread_tree
            .thread_group_leaders
            .iter()
            .copied()
            .filter(|process| self.thread_tree.process_group(*process) == Some(pgid))
            .filter(|process| {
                self.thread_tree
                    .my_thread_group(process)
                    .into_iter()
                    .any(|tid| self.next_turns.contains_key(&tid))
            })
            .collect();
        members.sort();
        self.reserve_sigkill_processes(sender, &members, uid)
    }

    fn reserve_sigkill_processes(
        &mut self,
        sender: DetTid,
        targets: &[DetPid],
        uid: u32,
    ) -> Result<SigkillReservation, SigkillRefusal> {
        if !retires_sigkill_at_the_kill_turn(&self.backend) {
            return Err(SigkillRefusal::Backend);
        }
        if targets.is_empty() {
            return Err(SigkillRefusal::NoTarget);
        }
        let mut victims = Vec::new();
        let mut includes_sender = false;
        for target in targets {
            if !self.thread_tree.thread_group_leaders.contains(target) {
                return Err(SigkillRefusal::NoTarget);
            }
            if self.thread_tree.is_pid_namespace_init(*target) {
                crate::detlog::write_loss_notice(&format!(
                    "a SIGKILL victim, process {target}, is a PID namespace's init, which Linux may protect from it, so the send keeps its host-timed retirement (https://github.com/rrnewton/hermit/issues/3994)"
                ));
                return Err(SigkillRefusal::NamespaceInit(*target));
            }
            // A prior terminal cause stands: a process whose exit was already
            // granted keeps that exit's status and notification.
            if self.thread_tree.granted_exits.contains(target) {
                continue;
            }
            let own = self.registered_process(sender) == Some(*target);
            includes_sender |= own;
            let mut threads: Vec<DetTid> = self
                .thread_tree
                .my_thread_group(target)
                .into_iter()
                .filter(|tid| self.next_turns.contains_key(tid))
                .collect();
            if threads.is_empty() {
                return Err(SigkillRefusal::NoTarget);
            }
            // The sender's own thread is not a victim: its send does not
            // return, and its death is covered as an `exit_group` caller's.
            threads.retain(|tid| *tid != sender);
            threads.sort();
            for tid in threads {
                if self.sigkill.reserved.contains_key(&tid) {
                    return Err(SigkillRefusal::AlreadyReserved(tid));
                }
                // A vfork parent whose child has not reached a release edge is
                // still in the kernel's vfork wait: its vfork has not returned,
                // so no result was or will be recorded for it, and the kill
                // replays as it recorded.
                let in_unreturned_vfork = self.vfork_barriers.contains_key(&tid)
                    && !self.released_vfork_barriers.contains(&tid);
                if self.recordreplay_modes
                    && self.blocked.external_io_blockers.contains_key(&tid)
                    && !in_unreturned_vfork
                {
                    // A recording made now could not replay this kill: the
                    // victim's operation may have a recorded result whose
                    // rejoin will never be consumed. Replay refuses it before
                    // starting the guest.
                    crate::detlog::record_replay_refusal(&format!(
                        "a SIGKILL victim, thread {tid}, had a background operation in flight during record or replay, so its death is retired at its host-timed deregistration (https://github.com/rrnewton/hermit/issues/3994)"
                    ));
                    return Err(SigkillRefusal::BackgroundOperation(tid));
                }
                let Some(mm) = self.thread_mm(tid) else {
                    crate::detlog::write_loss_notice(&format!(
                        "a SIGKILL victim, thread {tid}, has no recorded address-space identity, so its death is retired at its host-timed deregistration (https://github.com/rrnewton/hermit/issues/3994)"
                    ));
                    return Err(SigkillRefusal::UnknownAddressSpace(tid));
                };
                victims.push(SigkillVictim {
                    tid,
                    process: *target,
                    mm,
                });
            }
        }
        let token = self.sigkill.next_token;
        self.sigkill.next_token += 1;
        for victim in &victims {
            self.sigkill.reserved.insert(victim.tid, token);
        }
        self.sigkill.reservations.insert(
            token,
            Reservation {
                victims,
                uid,
                includes_sender,
            },
        );
        Ok(SigkillReservation {
            token,
            includes_sender,
        })
    }

    /// Route a consuming receipt of `tid` (its deregistration, after its final
    /// accounting): see [`SigkillReceipt`].
    pub(crate) fn route_sigkill_receipt(&mut self, tid: DetTid) -> SigkillReceipt {
        if self.sigkill.reserved.contains_key(&tid) {
            self.sigkill.held_receipts.insert(tid);
            self.sigkill.accounted.insert(tid);
            SigkillReceipt::Held
        } else if self.sigkill.unaccounted.remove(&tid) {
            self.sigkill.accounted.insert(tid);
            SigkillReceipt::Committed
        } else {
            SigkillReceipt::Ordinary
        }
    }

    /// Whether a SIGKILL victim's consuming receipt for `tid` was already
    /// counted, so this one is a duplicate that must change nothing.
    pub(crate) fn sigkill_receipt_already_counted(&self, tid: DetTid) -> bool {
        self.sigkill.accounted.contains(&tid)
    }

    /// A new thread registers on `tid`: an earlier victim's counted receipt no
    /// longer applies to it.
    pub(crate) fn forget_sigkill_receipt(&mut self, tid: DetTid) {
        self.sigkill.accounted.remove(&tid);
    }

    /// Commit (`sent`) or unwind the reservation `token` at the sending
    /// boundary. Returns the victims retired by the commit.
    pub(crate) fn commit_sigkill(&mut self, token: u64, sent: bool) -> Vec<SigkillVictim> {
        let Some(reservation) = self.sigkill.reservations.remove(&token) else {
            return Vec::new();
        };
        for victim in &reservation.victims {
            self.sigkill.reserved.remove(&victim.tid);
        }
        if !sent {
            // No kill happened: wakes and a receipt that arrived meanwhile
            // take the ordinary path now.
            let victims: BTreeSet<DetTid> = reservation.victims.iter().map(|v| v.tid).collect();
            self.run_retained_wakes_of(|tid| victims.contains(&tid));
            for victim in &reservation.victims {
                if self.sigkill.held_receipts.remove(&victim.tid) {
                    self.logically_kill_thread(&victim.tid, &victim.process, victim.mm);
                }
            }
            return Vec::new();
        }
        // A victim that died before the commit (its receipt is held) may have
        // had its exit reported published already. Retiring its leader
        // forgets that report, which an Exit grant wants; here the report
        // must still stand, or the notification below installs a hold that
        // nothing can release.
        let published: BTreeSet<DetPid> = reservation
            .victims
            .iter()
            .map(|victim| victim.process)
            .filter(|process| self.child_exit_publications_completed.contains(process))
            .collect();
        // Every victim process is marked dying, and its exit granted, before
        // any is retired: re-parenting an earlier victim's orphans must pass
        // over a subreaper that this same send kills, and an adoption of a
        // victim's orphans happens at this committed turn, not at a
        // host-timed deregistration, so it is no determinism loss.
        // A send that includes the sender commits at the sender's own Exit
        // grant, which already notified the sender's process's parent.
        let already_notified: BTreeSet<DetPid> = reservation
            .victims
            .iter()
            .map(|victim| victim.process)
            .filter(|process| self.thread_tree.granted_exits.contains(process))
            .collect();
        for victim in &reservation.victims {
            self.sigkill.dying.insert(victim.process);
            self.thread_tree.granted_exits.insert(victim.process);
            // A committed SIGKILL dooms its victim for the child-wait
            // deadlock check (https://github.com/rrnewton/hermit/issues/3904):
            // a reserved send does not reach the per-send notification.
            self.record_committed_sigkill(victim.tid, Some(victim.process));
        }
        for victim in &reservation.victims {
            info!(
                "[scheduler] retiring dettid {} of SIGKILLed process {} at the kill turn",
                victim.tid, victim.process
            );
            if !self.sigkill.held_receipts.remove(&victim.tid) {
                self.sigkill.unaccounted.insert(victim.tid);
            }
            self.logically_kill_thread(&victim.tid, &victim.process, victim.mm);
        }
        self.sigkill.dying.clear();
        let mut processes: Vec<DetPid> = reservation
            .victims
            .iter()
            .map(|victim| victim.process)
            .collect();
        processes.dedup();
        self.child_exit_publications_completed
            .extend(published.iter().copied());
        // The processes of one send die together, and the kernel publishes
        // their exits to a shared parent in an order the host chooses: they
        // form one cohort for the child-exit ledger, which decides which of
        // them the kernel's coalesced SIGCHLD names.
        let cohort = (processes.len() > 1).then_some(token);
        for process in processes {
            if !already_notified.contains(&process) {
                self.own_killed_child_notification(process, reservation.uid, cohort);
            }
        }
        // A report the notification did not consume is forgotten, as
        // retiring the leader intended.
        for process in &published {
            self.child_exit_publications_completed.remove(process);
        }
        reservation.victims
    }

    /// The parent's notification of `process`'s death by SIGKILL, owned at the
    /// kill turn as an Exit grant owns it: when Linux would discard the
    /// parent's SIGCHLD, a hold until the kernel's publication; otherwise
    /// Linux's siginfo (`CLD_KILLED`, `SIGKILL`) on the scheduler's copy, and
    /// the child-exit timer. The first copy delivered stands for the exit
    /// (`child_exit_sigchld`).
    fn own_killed_child_notification(&mut self, process: DetPid, uid: u32, cohort: Option<u64>) {
        let Some(parent) = self.thread_tree.parent_process(&process) else {
            return;
        };
        if !self.should_synthesize_child_exit_signal(parent) {
            return;
        }
        // A child whose exit signal is not SIGCHLD (a raw clone with exit
        // signal 0 or SIGUSR1) is not notified with SIGCHLD: Linux sends its
        // own exit signal, if any, and the kernel delivers that itself.
        if matches!(
            self.thread_tree.notification_signal(process),
            Some(signal) if signal != libc::SIGCHLD
        ) {
            return;
        }
        let notification = if self.backend.reports_child_exit_publication {
            self.classify_child_exit_notification(process, parent)
        } else {
            super::ChildExitNotification::Undecided
        };
        match notification {
            super::ChildExitNotification::None | super::ChildExitNotification::Discard => {
                if !self.child_exit_publications_completed.remove(&process) {
                    self.child_exit_publications_pending.insert(process);
                    self.sigkill.unpublished.insert(process);
                }
            }
            super::ChildExitNotification::Deliver | super::ChildExitNotification::Undecided => {
                // The kernel sends the parent its own SIGCHLD too, once the
                // tracer has reaped the victim. The fence waits for that
                // publication, and the sender waits for it before its `kill`
                // returns (`CommitSigkill`), so the kernel's copy is pending at
                // a point the schedule fixes and is delivered first; the
                // scheduler's copy then sends nothing (`child_exit_sigchld`).
                if self.backend.reports_child_exit_publication
                    && !self.child_exit_publications_completed.remove(&process)
                {
                    self.child_exit_publications_pending.insert(process);
                    self.sigkill.unpublished.insert(process);
                }
                self.child_exit_sigchld.exit_status.insert(
                    process,
                    super::child_exit_sigchld::ChildExitSiginfo {
                        code: libc::CLD_KILLED,
                        pid: process.as_raw(),
                        uid,
                        status: libc::SIGKILL,
                    },
                );
                let deadline = self.committed_time + super::LogicalTime::from_nanos(1);
                let parent_thread = if self.models_signal_targets {
                    self.thread_tree
                        .process_wait
                        .get(&process)
                        .map_or(parent, |metadata| metadata.wait_owner)
                } else {
                    parent
                };
                if self.backend.reports_child_exit_publication
                    && self.child_exit_sigchld.own(parent, process)
                    && let Some(cohort) = cohort
                {
                    self.child_exit_sigchld.join_cohort(process, cohort);
                }
                self.blocked.timed_waiters.insert_child_exit(
                    deadline,
                    process,
                    parent,
                    parent_thread,
                );
            }
        }
    }

    /// Whether a committed victim's physical exit is still unaccounted, so no
    /// turn may be selected yet.
    pub(crate) fn sigkill_fence_holds(&self) -> bool {
        !self.sigkill.unaccounted.is_empty()
            || self
                .sigkill
                .unpublished
                .iter()
                .any(|process| self.child_exit_publications_pending.contains(process))
    }

    /// The kernel published `process`'s exit: a victim's publication no
    /// longer holds the fence.
    pub(crate) fn note_sigkill_publication(&mut self, process: DetPid) {
        self.sigkill.unpublished.remove(&process);
    }

    /// Whether reservation `token` includes the sender's own process, so it is
    /// committed before it is sent and its victims cannot have died yet.
    pub(crate) fn sigkill_reservation_includes_sender(&self, token: u64) -> bool {
        self.sigkill
            .reservations
            .get(&token)
            .is_some_and(|reservation| reservation.includes_sender)
    }

    /// Whether a committed send's `victims` are settled: each one's physical
    /// exit is accounted and each one's exit is published to its parent, so
    /// the sender's `kill` may return.
    pub(crate) fn sigkill_settled(&self, victims: &[SigkillVictim]) -> bool {
        victims.iter().all(|victim| {
            !self.sigkill.unaccounted.contains(&victim.tid)
                && !self
                    .child_exit_publications_pending
                    .contains(&victim.process)
        })
    }

    /// Record that the SIGKILL fence, or a sender's `kill`, waited `waited`
    /// past the valve: the daemon loop refuses the run.
    pub(crate) fn refuse_unsettled_sigkill(&mut self, waited: std::time::Duration) {
        if self.sigkill_fence_refusal.is_none() {
            let refusal = SigkillFenceRefusal {
                unaccounted: self.sigkill.unaccounted.iter().copied().collect(),
                unpublished: self
                    .sigkill
                    .unpublished
                    .iter()
                    .copied()
                    .filter(|process| self.child_exit_publications_pending.contains(process))
                    .collect(),
                waited,
            };
            tracing::error!("[sigkill] {}", refusal);
            self.sigkill_fence_refusal = Some(refusal);
        }
    }

    /// Whether `process` is a victim of the SIGKILL being committed right now.
    pub(crate) fn is_dying_sigkill_victim(&self, process: DetPid) -> bool {
        self.sigkill.dying.contains(&process)
    }

    /// Whether `tid` is a committed victim whose physical exit is not
    /// accounted yet. Its ordinary requests are never answered: wherever one
    /// is at the commit (filed, admitted but not completed, or arriving
    /// later, like a background operation's late rejoin), answering it would
    /// resume a callback the kernel is ending. Only its consuming cleanup is
    /// admitted.
    pub(crate) fn is_unaccounted_sigkill_victim(&self, tid: DetTid) -> bool {
        self.sigkill.unaccounted.contains(&tid)
    }

    /// Retain a victim's non-empty owner-death wake batch instead of running
    /// it at receipt, and say whether it was retained. Selecting waiters,
    /// consuming the fuzz arm's selection randomness and logging the result
    /// all depend on order, and receipts arrive in host order. A retained
    /// batch runs when the cohort is complete; the victim's exit path does
    /// not wait for that.
    pub(crate) fn retain_sigkill_victim_wakes(
        &mut self,
        from: DetTid,
        wakes: &[(DetTid, FutexID)],
    ) -> bool {
        if wakes.is_empty()
            || !(self.sigkill.reserved.contains_key(&from)
                || self.sigkill.unaccounted.contains(&from))
        {
            return false;
        }
        let position = self.sigkill.wake_batches.entry(from).or_default();
        self.sigkill
            .retained_wakes
            .insert((from, *position), wakes.to_vec());
        *position += 1;
        true
    }

    /// Run the retained wake batches of every victim `select` names, in
    /// (victim, position) order, and log each result.
    fn run_retained_wakes_of(&mut self, select: impl Fn(DetTid) -> bool) {
        let keys: Vec<(DetTid, u64)> = self
            .sigkill
            .retained_wakes
            .keys()
            .filter(|(victim, _)| select(*victim))
            .copied()
            .collect();
        for key in keys {
            let wakes = self.sigkill.retained_wakes.remove(&key).unwrap();
            let counts = self.wake_futex_waiters_after_exit(&wakes);
            for ((owner, futex), count) in wakes.into_iter().zip(counts) {
                info!(
                    "[detcore, dtid {}] robust-list owner death woke {} waiter(s) on futex {:?} after physical exit",
                    owner, count, futex,
                );
            }
        }
        let retained = &self.sigkill.retained_wakes;
        self.sigkill
            .wake_batches
            .retain(|victim, _| retained.keys().any(|(tid, _)| tid == victim));
    }

    /// Once no committed victim's physical exit is outstanding, run every
    /// retained wake batch, before step2 drains anything.
    pub(crate) fn run_retained_sigkill_wakes(&mut self) {
        if !self.sigkill_fence_holds() && !self.sigkill.retained_wakes.is_empty() {
            self.run_retained_wakes_of(|_| true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// root 1 with children 2 (the sender) and 3 (the target, two threads: 3
    /// and 4), every thread live and registered with an address space.
    fn family() -> Scheduler {
        let mut scheduler = Scheduler::new(&Config::default());
        let [root, sender, target, worker] = [1, 2, 3, 4].map(DetPid::from_raw);
        scheduler.thread_tree.add_child(root, root, true);
        scheduler.thread_tree.add_child(root, sender, true);
        scheduler.thread_tree.add_child(root, target, true);
        scheduler.thread_tree.add_child(target, worker, false);
        for tid in [root, sender, target, worker] {
            super::super::test::register_known_thread_for(&mut scheduler, tid);
            scheduler.record_thread_mm(tid, MmId::initial(tid));
        }
        scheduler
    }

    fn live(scheduler: &Scheduler, tid: i32) -> bool {
        scheduler.next_turns.contains_key(&DetTid::from_raw(tid))
    }

    /// The commit retires every thread of the target at the sending turn, and
    /// holds turn selection until each one's deregistration arrives.
    #[test]
    fn a_committed_sigkill_retires_the_whole_target_and_fences_until_accounted() {
        let mut scheduler = family();
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
            .unwrap()
            .token;
        assert!(live(&scheduler, 3) && live(&scheduler, 4));

        let victims = scheduler.commit_sigkill(token, true);

        assert_eq!(
            victims.iter().map(|v| v.tid.as_raw()).collect::<Vec<_>>(),
            [3, 4]
        );
        assert!(!live(&scheduler, 3) && !live(&scheduler, 4));
        assert!(
            scheduler
                .logically_exited_processes
                .contains(&DetPid::from_raw(3))
        );
        assert!(scheduler.sigkill_fence_holds());
        assert_eq!(
            scheduler.route_sigkill_receipt(DetTid::from_raw(4)),
            SigkillReceipt::Committed
        );
        assert!(scheduler.sigkill_fence_holds());
        assert_eq!(
            scheduler.route_sigkill_receipt(DetTid::from_raw(3)),
            SigkillReceipt::Committed
        );
        assert!(
            scheduler.sigkill_fence_holds(),
            "the victim's exit is not yet published to its parent"
        );
        scheduler.complete_child_exit_publication(DetPid::from_raw(3));
        assert!(!scheduler.sigkill_fence_holds());
        // A duplicate receipt is ordinary, and changes nothing.
        assert_eq!(
            scheduler.route_sigkill_receipt(DetTid::from_raw(3)),
            SigkillReceipt::Ordinary
        );
    }

    /// The fence waits for the victims' publications only: another process's
    /// exit awaiting its publication does not hold turn selection.
    #[test]
    fn the_fence_waits_for_no_publication_but_the_victims() {
        let mut scheduler = family();
        let bystander = DetPid::from_raw(2);
        scheduler.child_exit_publications_pending.insert(bystander);
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
            .unwrap()
            .token;
        scheduler.commit_sigkill(token, true);
        for tid in [3, 4] {
            scheduler.route_sigkill_receipt(DetTid::from_raw(tid));
        }
        assert!(scheduler.sigkill_fence_holds());
        scheduler.complete_child_exit_publication(DetPid::from_raw(3));
        assert!(!scheduler.sigkill_fence_holds());
        assert!(
            scheduler
                .child_exit_publications_pending
                .contains(&bystander)
        );
    }

    /// A victim's receipt is counted once; a new thread registered on the same
    /// tid is a different thread, whose deregistration counts again.
    #[test]
    fn a_new_thread_on_a_victims_tid_is_accounted_afresh() {
        let mut scheduler = family();
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
            .unwrap()
            .token;
        scheduler.commit_sigkill(token, true);
        let tid = DetTid::from_raw(4);
        scheduler.route_sigkill_receipt(tid);
        assert!(scheduler.sigkill_receipt_already_counted(tid));
        // As the registration of a new thread does (`GlobalState`'s
        // registration handler).
        scheduler.forget_committed_exit(tid);
        assert!(
            !scheduler.sigkill_receipt_already_counted(tid),
            "the new thread inherits the old one's counted receipt"
        );
    }

    /// A victim's receipt that arrives while the sender is still in its send
    /// is held: it retires nothing, so the retirement still happens at the
    /// commit, and the fence does not wait for it again.
    #[test]
    fn an_early_receipt_is_held_until_the_commit() {
        let mut scheduler = family();
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
            .unwrap()
            .token;

        assert_eq!(
            scheduler.route_sigkill_receipt(DetTid::from_raw(4)),
            SigkillReceipt::Held
        );
        assert!(live(&scheduler, 4), "a held receipt retires nothing");

        scheduler.commit_sigkill(token, true);
        assert!(!live(&scheduler, 4));
        scheduler.route_sigkill_receipt(DetTid::from_raw(3));
        scheduler.complete_child_exit_publication(DetPid::from_raw(3));
        assert!(
            !scheduler.sigkill_fence_holds(),
            "the held receipt was counted"
        );
    }

    /// A failed send retires nothing; a receipt held meanwhile then takes the
    /// ordinary path.
    #[test]
    fn a_failed_send_unwinds_the_reservation() {
        let mut scheduler = family();
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
            .unwrap()
            .token;
        scheduler.route_sigkill_receipt(DetTid::from_raw(4));

        assert!(scheduler.commit_sigkill(token, false).is_empty());

        assert!(live(&scheduler, 3), "no kill happened");
        assert!(
            !live(&scheduler, 4),
            "the held receipt took the ordinary path"
        );
        assert!(!scheduler.sigkill_fence_holds());
        assert_eq!(
            scheduler.route_sigkill_receipt(DetTid::from_raw(3)),
            SigkillReceipt::Ordinary
        );
    }

    /// An unknown target, a missing address-space identity and a second
    /// reservation of the same victim are refused, and keep the ordinary path.
    /// A send to the sender's own single-threaded process reserves no victim
    /// but says it includes the sender.
    #[test]
    fn sends_the_protocol_cannot_cover_are_refused() {
        let mut scheduler = family();
        let [sender, target] = [2, 3].map(DetPid::from_raw);
        let own = scheduler.reserve_sigkill(sender, sender, 0).unwrap();
        assert!(own.includes_sender);
        scheduler.commit_sigkill(own.token, false);
        assert_eq!(
            scheduler.reserve_sigkill(sender, DetPid::from_raw(99), 0),
            Err(SigkillRefusal::NoTarget)
        );
        scheduler.thread_mms.remove(&DetTid::from_raw(4));
        assert_eq!(
            scheduler.reserve_sigkill(sender, target, 0),
            Err(SigkillRefusal::UnknownAddressSpace(DetTid::from_raw(4)))
        );
        scheduler.record_thread_mm(DetTid::from_raw(4), MmId::initial(DetTid::from_raw(4)));
        let _reserved = scheduler.reserve_sigkill(sender, target, 0).unwrap();
        assert_eq!(
            scheduler.reserve_sigkill(DetPid::from_raw(1), target, 0),
            Err(SigkillRefusal::AlreadyReserved(DetTid::from_raw(3)))
        );
    }

    /// A process-group SIGKILL reserves and retires every live member, in pid
    /// order; a group that contains the sender's own process is refused.
    #[test]
    fn a_process_group_sigkill_retires_every_member() {
        let mut scheduler = family();
        let [root, sender, target] = [1, 2, 3].map(DetPid::from_raw);
        let group = DetPid::from_raw(77);
        assert!(scheduler.thread_tree.set_process_group(target, group));
        assert!(scheduler.thread_tree.set_process_group(root, group));
        let reserved = scheduler.reserve_sigkill_group(sender, group, 0).unwrap();
        assert!(!reserved.includes_sender);
        let victims = scheduler.commit_sigkill(reserved.token, true);
        assert_eq!(
            victims.iter().map(|v| v.tid.as_raw()).collect::<Vec<_>>(),
            [1, 3, 4]
        );
        assert!(!live(&scheduler, 1) && !live(&scheduler, 3) && !live(&scheduler, 4));
        assert!(live(&scheduler, 2));

        // A group containing the sender: its other members are victims, and
        // the sender's own thread is not.
        let mut scheduler = family();
        assert!(scheduler.thread_tree.set_process_group(sender, group));
        assert!(scheduler.thread_tree.set_process_group(target, group));
        let reserved = scheduler.reserve_sigkill_group(sender, group, 0).unwrap();
        assert!(reserved.includes_sender);
        let victims = scheduler.commit_sigkill(reserved.token, true);
        assert_eq!(
            victims.iter().map(|v| v.tid.as_raw()).collect::<Vec<_>>(),
            [3, 4]
        );
        assert!(live(&scheduler, 2));
    }

    /// A PID namespace's init may ignore the SIGKILL, so a send that would
    /// kill one keeps the ordinary path.
    #[test]
    fn a_pid_namespace_init_is_not_reserved() {
        let mut scheduler = family();
        let [sender, target] = [2, 3].map(DetPid::from_raw);
        scheduler.thread_tree.mark_pid_namespace_init(target);
        assert_eq!(
            scheduler.reserve_sigkill(sender, target, 0),
            Err(SigkillRefusal::NamespaceInit(target))
        );
        assert!(live(&scheduler, 3));
    }

    /// A backend that reports deaths another way is not covered.
    #[test]
    fn backends_with_their_own_death_reports_are_refused() {
        let config = Config::default().with_backend(|backend| {
            backend.requires_thread_directed_process_signals = true;
        });
        let mut scheduler = Scheduler::new(&config);
        let [root, target] = [1, 3].map(DetPid::from_raw);
        scheduler.thread_tree.add_child(root, root, true);
        scheduler.thread_tree.add_child(root, target, true);
        assert_eq!(
            scheduler.reserve_sigkill(root, target, 0),
            Err(SigkillRefusal::Backend)
        );
    }

    /// Two victims of one send each deliver an owner-death batch, for
    /// different futexes with three surviving waiters each, in either order,
    /// with a step2 between the receipts (design revision 4, R2). The waiters
    /// woken, the run-queue order and the fuzz arm's next draw must not depend
    /// on the order, under FIFO and under the fuzz arm.
    ///
    /// This verifies the retention protocol, not a SIGKILL producer: on
    /// ptrace today a SIGKILL victim stages no owner-death batch (the
    /// fatal-signal case of https://github.com/rrnewton/hermit/issues/2082),
    /// so the batches are handed to the scheduler as
    /// `GlobalState::recv_robust_list_wakes` would hand them.
    #[test]
    fn victims_owner_death_wakes_do_not_depend_on_receipt_order() {
        let mm = MmId::initial(DetTid::from_raw(3));
        let futexes = [0x404100, 0x404200].map(|addr| FutexID::private(mm, addr));
        let run = |fuzz: bool, reverse: bool| {
            let mut scheduler = family();
            scheduler.fuzz_futexes = fuzz;
            for (futex, waiters) in futexes.iter().zip([[11, 12, 13], [14, 15, 16]]) {
                for raw in waiters {
                    let tid = DetTid::from_raw(raw);
                    super::super::test::register_known_thread_for(&mut scheduler, tid);
                    scheduler.sleep_futex_waiter(&tid, *futex, None, u32::MAX, None);
                }
            }
            let token = scheduler
                .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
                .unwrap()
                .token;
            scheduler.commit_sigkill(token, true);
            let mut receipts = [
                (DetTid::from_raw(3), futexes[0]),
                (DetTid::from_raw(4), futexes[1]),
            ];
            if reverse {
                receipts.reverse();
            }
            for (victim, futex) in receipts {
                // As `GlobalState::recv_robust_list_wakes` does.
                if !scheduler.retain_sigkill_victim_wakes(victim, &[(victim, futex)]) {
                    scheduler.wake_futex_waiters_after_exit(&[(victim, futex)]);
                }
                let _ = scheduler.step2_drain_prefix();
                assert_eq!(
                    scheduler.route_sigkill_receipt(victim),
                    SigkillReceipt::Committed
                );
                let _ = scheduler.step2_drain_prefix();
            }
            assert!(scheduler.sigkill_fence_holds(), "the exit is unpublished");
            scheduler.complete_child_exit_publication(DetPid::from_raw(3));
            let _ = scheduler.step2_drain_prefix();
            let still_waiting: Vec<Vec<DetTid>> = futexes
                .iter()
                .map(|futex| {
                    scheduler
                        .blocked
                        .futex_waiters
                        .get(futex)
                        .map(|waiters| waiters.iter().map(|w| w.dettid).collect())
                        .unwrap_or_default()
                })
                .collect();
            let queue: Vec<DetTid> = scheduler.run_queue.tids().copied().collect();
            let next_draw: u64 = rand::RngExt::random(&mut scheduler.fuzz_prng);
            (still_waiting, queue, next_draw)
        };
        for fuzz in [false, true] {
            let forward = run(fuzz, false);
            assert_eq!(forward.0.iter().map(Vec::len).collect::<Vec<_>>(), [2, 2]);
            assert_eq!(forward, run(fuzz, true), "fuzz={fuzz}");
        }
    }

    /// In record and replay, a victim with a background operation in flight
    /// is refused: its result may be recorded with a rejoin that is never
    /// consumed. A vfork parent whose vfork has not returned is the exception:
    /// its child has not reached a release edge, so no result exists. Once the
    /// child releases it, the vfork returns, and the refusal applies again.
    #[test]
    fn record_mode_refuses_a_background_victim_except_an_unreturned_vfork() {
        let victim = DetTid::from_raw(3);
        for (case, refused) in [
            ("background read", true),
            ("vfork not returned", false),
            ("vfork released", true),
        ] {
            let mut scheduler = family();
            scheduler.recordreplay_modes = true;
            scheduler
                .blocked
                .external_io_blockers
                .insert(victim, crate::resources::ExternalOpId::new(victim, 1));
            match case {
                "vfork not returned" => {
                    scheduler.vfork_barriers.insert(victim, None);
                }
                "vfork released" => {
                    scheduler
                        .vfork_barriers
                        .insert(victim, Some(DetTid::from_raw(9)));
                    scheduler.released_vfork_barriers.insert(victim);
                }
                _ => {}
            }
            let reserved = scheduler.reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0);
            assert_eq!(
                matches!(reserved, Err(SigkillRefusal::BackgroundOperation(tid)) if tid == victim),
                refused,
                "{case}: {reserved:?}"
            );
        }
    }

    /// A SIGKILLed raw clone child whose exit signal is SIGUSR1 or 0 gets no
    /// synthetic SIGCHLD and no publication hold: Linux notifies its parent
    /// with its own exit signal, if any, which the kernel delivers.
    #[test]
    fn a_killed_child_with_a_non_sigchld_exit_signal_gets_no_synthetic_sigchld() {
        for exit_signal in [libc::SIGUSR1, 0] {
            let mut scheduler = Scheduler::new(&Config::default());
            let [root, sender, child] = [1, 2, 3].map(DetPid::from_raw);
            scheduler.thread_tree.add_child(root, root, true);
            scheduler.thread_tree.add_child(root, sender, true);
            scheduler.thread_tree.add_child_with_wait_metadata(
                root,
                child,
                true,
                false,
                exit_signal,
            );
            for tid in [root, sender, child] {
                super::super::test::register_known_thread_for(&mut scheduler, tid);
                scheduler.record_thread_mm(tid, MmId::initial(tid));
            }
            let token = scheduler.reserve_sigkill(sender, child, 0).unwrap().token;
            scheduler.commit_sigkill(token, true);
            let timer_armed = scheduler.blocked.timed_waiters.iter().any(|(_, event)| {
                matches!(
                    event,
                    super::super::TimedEvent::SignalEvt(
                        super::super::timed_waiters::SignalTimerId::ChildExit { child: exited, .. },
                        ..
                    ) if exited == child
                )
            });
            assert!(!timer_armed, "exit signal {exit_signal}");
            assert!(
                scheduler.child_exit_publications_pending.is_empty(),
                "exit signal {exit_signal}"
            );
        }
    }

    /// A committed SIGKILL dooms each victim process for the child-wait
    /// deadlock check of https://github.com/rrnewton/hermit/issues/3904, as
    /// the per-send notification does for a send that is not reserved.
    #[test]
    fn a_committed_sigkill_dooms_its_victims_for_the_deadlock_check() {
        let mut scheduler = family();
        let target = DetPid::from_raw(3);
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), target, 0)
            .unwrap()
            .token;
        assert_eq!(scheduler.sigkill_recorded(target), (false, false));
        scheduler.commit_sigkill(token, true);
        assert_eq!(scheduler.sigkill_recorded(target), (true, false));
    }

    /// A fence still waiting for a victim's death past the valve refuses the
    /// run by name instead of hanging.
    #[test]
    fn a_sigkill_fence_past_its_valve_refuses_the_run() {
        let mut scheduler = family();
        let token = scheduler
            .reserve_sigkill(DetTid::from_raw(2), DetPid::from_raw(3), 0)
            .unwrap()
            .token;
        scheduler.commit_sigkill(token, true);
        assert!(scheduler.step2_drain_prefix().is_err());
        assert!(scheduler.sigkill_fence_refusal.is_none());
        scheduler.sigkill_fence_wait_since = std::time::Instant::now()
            .checked_sub(Scheduler::SIGKILL_FENCE_VALVE + std::time::Duration::from_secs(1));
        assert!(scheduler.step2_drain_prefix().is_err());
        let refusal = scheduler
            .sigkill_fence_refusal
            .take()
            .expect("the fence must refuse past its valve");
        assert_eq!(
            refusal.unaccounted,
            [DetTid::from_raw(3), DetTid::from_raw(4)]
        );
        assert!(refusal.waited >= Scheduler::SIGKILL_FENCE_VALVE);
        assert!(
            refusal
                .to_string()
                .starts_with("hermit refused to continue the run")
        );
    }
}
