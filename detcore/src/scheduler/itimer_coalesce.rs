/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's coalescing of a periodic `ITIMER_REAL` on the traced path
//! (<https://github.com/rrnewton/hermit/issues/4139>).
//!
//! Linux's `it_real_fn` sends SIGALRM to the thread-group leader and returns
//! `HRTIMER_NORESTART`. Only a dequeue of a SIGALRM from the process's shared
//! queue, whatever sent it, restarts the timer (`posixtimer_rearm_itimer`,
//! `hrtimer_forward_now` on the original grid). So at most one timer SIGALRM is
//! outstanding, missed periods are dropped, and `getitimer` reads 0 while the
//! timer waits. Before this, the traced path re-armed every expiry at the old
//! deadline plus one interval, so a slice that crossed N periods fired N
//! SIGALRMs at its end, one per scheduler pass.
//!
//! - **Observable processes.** The scheduler re-arms at a dequeue only where it
//!   can see the dequeue at a committed point: no thread of the process has run
//!   outside the schedule (`background_call_processes`), no host-timed source
//!   can post SIGALRM to it (`host_timed_signals`, the unreaped children's exit
//!   signals), and its leader is live. Those conditions are sticky once false
//!   (`alarm_unobservable`). Every other process forwards each expiry to the
//!   first grid point after the pop: missed periods are still dropped, but the
//!   timer does not wait for the dequeue.
//! - **The pop.** An observable process's expiry that its leader ignores at
//!   generation (`sig_ignored`: SIG_IGN, or SIG_DFL in a namespace init, and
//!   not blocked by the leader) sends nothing and leaves the timer `Inactive`
//!   with no copy, as Linux discards it. Otherwise the scheduler sends the
//!   signal and the timer becomes `Inactive` with a copy.
//! - **Reconcile (O3).** At each quiesced boundary, before the next pass pops
//!   or grants, an `Inactive` timer with a copy whose process's shared queue no
//!   longer holds SIGALRM re-arms at the first grid point after the committed
//!   time. Between two quiesced boundaries only the granted thread of an
//!   observable process runs, so that read is a function of the schedule.
//!
//! Record and replay keep the legacy recurrence and return values
//! (`recordreplay_modes`), so recordings keep their event stream.

use nix::sys::signal::Signal;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::info;

use super::Scheduler;
use super::exit_on_scheduler_refusal;
use super::kernel_signal_bit;
use super::timed_waiters::AlarmPhase;
use super::timed_waiters::SignalTimerId;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// Which processes a guest's committed SIGALRM send could have queued a
/// shared-queue copy for (rule 4).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GuestSigalrmTarget {
    /// `kill(pid > 0)`, `rt_sigqueueinfo(tgid)`.
    Process(DetPid),
    /// `kill(0)` (the sender's group, `None`) or `kill(-pgrp)`.
    Group(Option<DetPid>),
    /// `kill(-1)`.
    All,
    /// A send whose targets Detcore cannot classify (a nested PID namespace's
    /// pid, a pidfd): every process it might reach stops being observable.
    Unclassified,
}

/// What a read of a process's shared queue says about its ITIMER_REAL copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SharedCopy {
    Dequeued,
    Pending,
    ProcessGone,
    Unreadable,
}

/// The bit of SIGALRM in a kernel signal set.
fn sigalrm_bit() -> u64 {
    kernel_signal_bit(libc::SIGALRM)
}

impl Scheduler {
    /// Whether this scheduler coalesces periodic `ITIMER_REAL` expiries at all:
    /// only on the traced path, which models signal targets, outside record and
    /// replay, and not where a backend owns process signal control (that one
    /// has its own dequeue-time re-arm, `real_timer.rs`).
    pub(super) fn coalesces_alarms(&self) -> bool {
        self.models_signal_targets && !self.recordreplay_modes && !self.signal_control_installed()
    }

    /// Whether the current process state permits identifying a shared-queue
    /// SIGALRM dequeue. A metadata query must not commit a recurrence-policy
    /// change during a transient state, such as an exited leader before exec.
    pub(super) fn alarm_observable_now(&self, process: DetPid) -> bool {
        if !self.coalesces_alarms() {
            return false;
        }
        if self.alarm_unobservable.contains(&process) {
            return false;
        }
        let bit = sigalrm_bit();
        let threads = self.process_signal_targets(process);
        let leader = DetTid::from_raw(process.as_raw());
        let host_timed = threads
            .iter()
            .any(|thread| self.host_timed_signals(*thread) & bit != 0);
        let foreign_exit = self
            .thread_tree
            .unreaped_children_of(process)
            .into_iter()
            .filter_map(|child| self.thread_tree.notification_signal(child))
            .any(|signal| signal == libc::SIGALRM);
        !self.background_call_processes.contains(&process)
            && !host_timed
            && !foreign_exit
            && threads.contains(&leader)
    }

    /// The recurrence owner commits loss of observability at its existing
    /// timer boundaries. Once committed, it remains sticky.
    pub(super) fn alarm_observable(&mut self, process: DetPid) -> bool {
        if !self.coalesces_alarms() || self.alarm_unobservable.contains(&process) {
            return false;
        }
        let observable = self.alarm_observable_now(process);
        if !observable {
            debug!(
                "[dpid {}] ITIMER_REAL dequeues are not observable from here on; expiries forward at the pop.",
                process
            );
            self.alarm_unobservable.insert(process);
        }
        observable
    }

    /// Whether `process`'s leader ignores SIGALRM at generation, as Linux's
    /// `sig_ignored(leader)` decides: the disposition is SIG_IGN, or SIG_DFL
    /// when the process is a guest namespace init (`sig_task_ignored` discards
    /// that for a `SIGNAL_UNKILLABLE` task), and the leader's committed mask
    /// does not block it. `None` when the leader's state cannot be read.
    fn leader_ignores_sigalrm(&self, process: DetPid) -> Option<bool> {
        let leader = DetTid::from_raw(process.as_raw());
        let state = self.read_sleeper_signal_state(leader).ok()?;
        let bit = sigalrm_bit();
        let mask = self
            .saved_guest_sigmasks
            .get(&leader)
            .copied()
            .unwrap_or(state.blocked);
        let sig_ign = state.ignored & bit != 0;
        let sig_dfl_init = self.thread_tree.is_pid_namespace_init(process)
            && state.ignored & bit == 0
            && state.caught & bit == 0;
        Some((sig_ign || sig_dfl_init) && mask & bit == 0)
    }

    /// Fire the expiry at `deadline` of `process`'s `ITIMER_REAL`, which
    /// `pop_if_before` has just popped (and, if periodic, re-armed one interval
    /// later), then replace that legacy recurrence.
    pub(super) fn fire_itimer_real(
        &mut self,
        deadline: LogicalTime,
        process: DetPid,
        tid: DetTid,
        signal: Signal,
    ) {
        let periodic = self
            .blocked
            .timed_waiters
            .signal_timer_period(SignalTimerId::Alarm(process))
            .is_some();
        if !periodic || !self.coalesces_alarms() {
            self.fire_itimer_real_copy(process, tid, signal);
            return;
        }
        let now = self.committed_time;
        if self.exec_teardowns.contains_key(&process) {
            // The leader's mask cannot be read from `/proc` during `de_thread`:
            // hold the expiry and decide it when the exec barrier ends
            // (`finish_exec_alarm`), keeping the original grid.
            self.blocked
                .timed_waiters
                .alarm_after_fire(process, deadline, Some(false), now);
            self.alarm_exec_deferred.insert(process, (tid, signal));
            return;
        }
        if !self.alarm_observable(process) {
            self.fire_itimer_real_copy(process, tid, signal);
            self.blocked
                .timed_waiters
                .alarm_after_fire(process, deadline, None, now);
            return;
        }
        match self.leader_ignores_sigalrm(process) {
            Some(true) => {
                info!(
                    "[dpid {}] SIGALRM expired while its leader ignores it; discarded at generation, the timer waits for a dequeue.",
                    process
                );
                self.blocked
                    .timed_waiters
                    .alarm_after_fire(process, deadline, Some(false), now);
            }
            Some(false) => {
                self.guest_send_landed = None;
                let sent = self.fire_itimer_real_copy(process, tid, signal);
                let landed = sent && self.guest_send_landed != Some(false);
                let copy = landed.then_some(true);
                self.blocked
                    .timed_waiters
                    .alarm_after_fire(process, deadline, copy, now);
            }
            None => {
                self.alarm_unobservable.insert(process);
                self.fire_itimer_real_copy(process, tid, signal);
                self.blocked
                    .timed_waiters
                    .alarm_after_fire(process, deadline, None, now);
            }
        }
    }

    /// At the end of `process`'s exec barrier, decide an expiry that popped
    /// during it, then run the reconcile the barrier deferred. Both use the
    /// committed time at this point.
    pub(super) fn finish_exec_alarm(&mut self, process: DetPid) {
        let t = self.committed_time;
        if let Some((tid, signal)) = self.alarm_exec_deferred.remove(&process) {
            let observable = self.alarm_observable(process);
            let ignored = if observable {
                self.leader_ignores_sigalrm(process)
            } else {
                None
            };
            match ignored {
                Some(true) => {}
                Some(false) => {
                    self.guest_send_landed = None;
                    let sent = self.fire_itimer_real_copy(process, tid, signal);
                    if sent && self.guest_send_landed != Some(false) {
                        self.blocked.timed_waiters.alarm_set_copy(process, true);
                    } else {
                        self.blocked.timed_waiters.alarm_rearm_after(process, t);
                    }
                }
                None => {
                    if observable {
                        self.alarm_unobservable.insert(process);
                    }
                    self.fire_itimer_real_copy(process, tid, signal);
                    self.blocked.timed_waiters.alarm_rearm_after(process, t);
                }
            }
        }
        self.reconcile_alarm(process, t);
    }

    /// `process`'s shared pending set, read through the thread group
    /// (`/proc/<tgid>/status`, valid while any thread lives).
    fn read_process_shared_pending(&self, process: DetPid) -> Option<u64> {
        #[cfg(test)]
        {
            let leader = DetTid::from_raw(process.as_raw());
            self.read_shared_pending(leader)
        }
        #[cfg(not(test))]
        {
            let status =
                std::fs::read_to_string(format!("/proc/{}/status", process.as_raw())).ok()?;
            crate::syscalls::KernelSignalState::parse(&status).map(|state| state.shared_pending)
        }
    }

    /// Reconcile `process`'s `Inactive` `ITIMER_REAL` at the committed time
    /// `t`: re-arm it when its copy has left the shared queue, or when the
    /// process stopped being observable (rule 6). Returns the new deadline if
    /// it re-armed.
    pub(crate) fn reconcile_alarm(
        &mut self,
        process: DetPid,
        t: LogicalTime,
    ) -> Option<LogicalTime> {
        let AlarmPhase::Inactive { copy } = self.blocked.timed_waiters.alarm_phase(process)? else {
            return None;
        };
        if self.exec_teardowns.contains_key(&process) {
            // `/proc/<tgid>` can be briefly unresolvable during `de_thread`;
            // the barrier's end reconciles.
            return None;
        }
        if !self.alarm_observable(process) {
            let next = self.blocked.timed_waiters.alarm_rearm_after(process, t);
            debug!(
                "[dpid {}] ITIMER_REAL re-armed at {:?}: its process is no longer observable.",
                process, next
            );
            return next;
        }
        // Invariant: the queue is read only when a copy is known to be there.
        // Under ptrace a guest kill() to a leader that ignores SIGALRM leaves a
        // physical copy native Linux drops; this read must not count it.
        if !copy {
            return None;
        }
        match self.shared_copy_outcome(process) {
            SharedCopy::Dequeued => {
                let next = self.blocked.timed_waiters.alarm_rearm_after(process, t);
                debug!(
                    "[dpid {}] SIGALRM left the shared queue; ITIMER_REAL re-armed at {:?}.",
                    process, next
                );
                next
            }
            SharedCopy::Pending => None,
            SharedCopy::ProcessGone => {
                self.blocked.timed_waiters.remove_alarm(process);
                None
            }
            SharedCopy::Unreadable => exit_on_scheduler_refusal(&format_args!(
                "HERMIT_SIGALRM_QUEUE_UNREADABLE: the shared signal queue of process {} could not be read while it has a live thread, so whether its ITIMER_REAL copy was dequeued is unknown (https://github.com/rrnewton/hermit/issues/4139)",
                process
            )),
        }
    }

    /// What `process`'s shared queue says about its ITIMER_REAL copy. A read
    /// that fails while the process has a committed-live thread is
    /// `Unreadable`, which refuses the run; one that fails after its exit
    /// committed is `ProcessGone`.
    fn shared_copy_outcome(&self, process: DetPid) -> SharedCopy {
        match self.read_process_shared_pending(process) {
            Some(pending) if pending & sigalrm_bit() == 0 => SharedCopy::Dequeued,
            Some(_) => SharedCopy::Pending,
            None if self.process_signal_targets(process).is_empty() => SharedCopy::ProcessGone,
            None => SharedCopy::Unreadable,
        }
    }

    /// Rule 4: a guest's committed SIGALRM send from `sender`. An `Inactive`
    /// timer with no known copy (its last expiry was discarded at
    /// generation) now has one, unless the leader still ignores SIGALRM at
    /// generation, so the dequeue of that copy restarts it, as Linux restarts
    /// a timer on any shared-queue SIGALRM dequeue.
    pub(crate) fn note_guest_sigalrm(&mut self, sender: DetTid, target: GuestSigalrmTarget) {
        if !self.coalesces_alarms() {
            return;
        }
        let inactive = self.blocked.timed_waiters.inactive_alarm_processes();
        let targets: Vec<DetPid> = match target {
            GuestSigalrmTarget::Process(process) => vec![process],
            GuestSigalrmTarget::Group(group) => {
                let group =
                    group.or_else(|| self.thread_tree.process_group(self.sigchld_process(sender)));
                match group {
                    Some(group) => inactive
                        .iter()
                        .copied()
                        .filter(|process| self.thread_tree.process_group(*process) == Some(group))
                        .collect(),
                    None => {
                        self.alarm_unobservable.extend(inactive.iter().copied());
                        return;
                    }
                }
            }
            GuestSigalrmTarget::All => inactive.clone(),
            GuestSigalrmTarget::Unclassified => {
                self.alarm_unobservable.extend(inactive.iter().copied());
                return;
            }
        };
        for process in targets {
            if self.blocked.timed_waiters.alarm_phase(process)
                != Some(AlarmPhase::Inactive { copy: false })
            {
                continue;
            }
            match self.leader_ignores_sigalrm(process) {
                Some(false) => self.blocked.timed_waiters.alarm_set_copy(process, true),
                Some(true) => {}
                None => {
                    self.alarm_unobservable.insert(process);
                }
            }
        }
    }

    /// Rule 5: `caller` changed SIGALRM's disposition. Setting SIG_IGN flushes
    /// a pending copy from every queue without dequeuing it, so an `Inactive`
    /// timer's copy is gone and the timer stays inactive (`sig_handler_ignored`;
    /// SIG_DFL never flushes SIGALRM). Decided from the disposition the kernel
    /// installed, not the call's return value.
    pub(crate) fn note_sigalrm_disposition(&mut self, caller: DetTid) {
        if !self.coalesces_alarms() {
            return;
        }
        let process = self.sigchld_process(caller);
        if self.blocked.timed_waiters.alarm_phase(process)
            != Some(AlarmPhase::Inactive { copy: true })
        {
            return;
        }
        match self.read_sleeper_signal_state(caller) {
            Ok(state) if state.ignored & sigalrm_bit() != 0 => {
                self.blocked.timed_waiters.alarm_set_copy(process, false);
            }
            Ok(_) => {}
            Err(_) => {
                self.alarm_unobservable.insert(process);
            }
        }
    }

    /// O3: at a quiesced boundary, reconcile every `Inactive` `ITIMER_REAL`.
    pub(super) fn reconcile_inactive_alarms(&mut self, t: LogicalTime) {
        if !self.coalesces_alarms() {
            return;
        }
        for process in self.blocked.timed_waiters.inactive_alarm_processes() {
            self.reconcile_alarm(process, t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::scheduler::HostTimedSignalScope;
    use crate::scheduler::test::register_known_thread_for;
    use crate::scheduler::timed_waiters::SignalTarget;
    use crate::syscalls::KernelSignalState;

    const PERIOD: u64 = 1_000_000;

    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    /// A traced scheduler with one single-threaded process 100 whose
    /// periodic 1 ms ITIMER_REAL first expires at 1 ms, and whose leader
    /// neither blocks nor ignores SIGALRM.
    fn one_process() -> (Scheduler, DetPid, DetTid) {
        let mut scheduler = Scheduler::new(
            &Config {
                sequentialize_threads: true,
                ..Config::default()
            }
            .with_backend(|backend| {
                detcore_model::config::set_kernel_signal_facts(backend, true);
                backend.delivers_preemption_timer_events = false;
                backend.runs_guest_code_between_injections = false;
            }),
        );
        assert!(scheduler.coalesces_alarms());
        scheduler.test_guest_sends_land = true;
        let leader = DetTid::from_raw(100);
        let process = DetPid::from_raw(100);
        scheduler.thread_tree.add_child(leader, leader, true);
        register_known_thread_for(&mut scheduler, leader);
        set_state(&mut scheduler, leader, 0, 0, 0);
        scheduler.register_alarm(
            process,
            leader,
            at(0),
            at(PERIOD),
            at(PERIOD),
            Signal::SIGALRM,
        );
        (scheduler, process, leader)
    }

    fn set_state(
        scheduler: &mut Scheduler,
        thread: DetTid,
        shared: u64,
        blocked: u64,
        ignored: u64,
    ) {
        scheduler.test_kernel_signal_states.insert(
            thread,
            KernelSignalState {
                shared_pending: shared,
                pending: shared,
                blocked,
                ignored,
                ..Default::default()
            },
        );
    }

    /// Pop and dispatch the next timed event at committed time `now`.
    fn pop_at(scheduler: &mut Scheduler, now: u64) {
        scheduler.committed_time = at(now);
        let (time, evt) = scheduler
            .blocked
            .timed_waiters
            .pop_if_before(at(now))
            .expect("a due expiry");
        let super::super::timed_waiters::TimedEvent::SignalEvt(id, target, sig) = evt else {
            panic!("expected a signal timer, got {evt}");
        };
        scheduler.dispatch_timed_signal(time, id, target, sig, true, at(now));
    }

    fn phase(scheduler: &Scheduler, process: DetPid) -> Option<AlarmPhase> {
        scheduler.blocked.timed_waiters.alarm_phase(process)
    }

    /// A pop sends the signal and leaves the timer Inactive with a copy, with
    /// no deadline queued however many periods the slice crossed; the copy's
    /// departure from the shared queue re-arms it on the original grid at the
    /// next quiesced boundary. While the copy is still pending nothing moves.
    #[test]
    fn a_pop_waits_for_the_dequeue_and_re_arms_on_the_grid() {
        let (mut scheduler, process, leader) = one_process();
        // The slice crossed 5.5 periods before this pop.
        pop_at(&mut scheduler, 5_500_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: true })
        );
        assert!(
            scheduler.blocked.timed_waiters.is_empty(),
            "no backlog of expiries"
        );
        // Still pending: no change.
        set_state(&mut scheduler, leader, sigalrm_bit(), 0, 0);
        assert_eq!(scheduler.reconcile_alarm(process, at(5_800_000)), None);
        // Dequeued: re-armed at the first grid point after t.
        set_state(&mut scheduler, leader, 0, 0, 0);
        assert_eq!(
            scheduler.reconcile_alarm(process, at(7_200_000)),
            Some(at(8_000_000))
        );
        assert_eq!(phase(&scheduler, process), Some(AlarmPhase::Active));
        assert_eq!(
            scheduler.blocked.timed_waiters.next_deadline(),
            Some(at(8_000_000))
        );
    }

    /// The leader ignores SIGALRM and does not block it: Linux discards the
    /// expiry at generation, so nothing is sent, the timer waits with no copy,
    /// and the empty queue does not re-arm it.
    #[test]
    fn an_expiry_the_leader_ignores_is_discarded_at_generation() {
        let (mut scheduler, process, leader) = one_process();
        set_state(&mut scheduler, leader, 0, 0, sigalrm_bit());
        let attempts = scheduler.host_signal_attempts;
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(scheduler.host_signal_attempts, attempts, "nothing is sent");
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: false })
        );
        assert_eq!(scheduler.reconcile_alarm(process, at(9_000_000)), None);
    }

    /// The leader blocks SIGALRM, so SIG_IGN does not discard it at
    /// generation (Linux tests the leader's `blocked`): the copy is queued.
    #[test]
    fn a_blocking_leader_keeps_an_ignored_expiry() {
        let (mut scheduler, process, leader) = one_process();
        set_state(&mut scheduler, leader, 0, sigalrm_bit(), sigalrm_bit());
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: true })
        );
    }

    /// SIG_DFL SIGALRM to a guest namespace init is discarded at generation
    /// (`SIGNAL_UNKILLABLE`).
    #[test]
    fn a_namespace_init_discards_a_default_sigalrm() {
        let (mut scheduler, process, _leader) = one_process();
        scheduler.thread_tree.mark_pid_namespace_init(process);
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: false })
        );
    }

    /// A send that does not land (ESRCH) forwards instead of waiting for a
    /// copy that was never queued.
    #[test]
    fn a_send_that_does_not_land_forwards() {
        let (mut scheduler, process, _leader) = one_process();
        scheduler.test_guest_sends_land = false;
        scheduler.backend.requires_thread_directed_process_signals = true;
        pop_at(&mut scheduler, 2_500_000);
        assert_eq!(phase(&scheduler, process), Some(AlarmPhase::Active));
        assert_eq!(
            scheduler.blocked.timed_waiters.next_deadline(),
            Some(at(3_000_000))
        );
    }

    /// A process one of whose threads ran a call outside the schedule, or for
    /// which a host-timed source can post SIGALRM, is not observable for the
    /// rest of the run: its pops forward, and an Inactive timer is re-armed at
    /// the committed point where that is noticed (rule 6), copy or not.
    #[test]
    fn an_unobservable_process_forwards_and_re_arms() {
        let (mut scheduler, process, _leader) = one_process();
        scheduler.background_call_processes.insert(process);
        pop_at(&mut scheduler, 3_200_000);
        assert_eq!(
            scheduler.blocked.timed_waiters.next_deadline(),
            Some(at(4_000_000))
        );

        let (mut scheduler, process, leader) = one_process();
        pop_at(&mut scheduler, 1_000_000);
        scheduler
            .blocked
            .timed_waiters
            .alarm_set_copy(process, false);
        scheduler.record_host_timed_signals(leader, HostTimedSignalScope::Container, sigalrm_bit());
        assert_eq!(
            scheduler.reconcile_alarm(process, at(1_500_000)),
            Some(at(2_000_000))
        );
        assert!(scheduler.alarm_unobservable.contains(&process));
        // Sticky even if the record could shrink.
        assert!(!scheduler.alarm_observable(process));
    }

    /// Inspecting ordinary reported-signal metadata while the leader is
    /// absent must not latch a timer recurrence decision. Exec can later
    /// reconnect the leader before any periodic timer has needed that decision.
    #[test]
    fn reported_metadata_during_a_leader_gap_does_not_change_recurrence() {
        use super::super::discarded_at_send::DiscardedSignalAnswer;
        use super::super::discarded_at_send::DiscardedSignalControl;

        let (mut scheduler, process, leader) = one_process();
        let worker = DetTid::from_raw(101);
        scheduler.thread_tree.add_child(leader, worker, false);
        register_known_thread_for(&mut scheduler, worker);
        scheduler.next_turns.remove(&leader);
        assert!(!scheduler.alarm_observable_now(process));
        assert_eq!(
            scheduler.discarded_signal_control(worker, DiscardedSignalControl::ClaimItimerReal),
            DiscardedSignalAnswer::ItimerReal(false)
        );
        assert!(!scheduler.alarm_unobservable.contains(&process));
        register_known_thread_for(&mut scheduler, leader);
        assert!(scheduler.alarm_observable(process));
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: true })
        );
    }

    /// Record and replay keep the legacy recurrence and return values.
    #[test]
    fn record_and_replay_keep_the_legacy_recurrence() {
        let (mut scheduler, process, _leader) = one_process();
        scheduler.recordreplay_modes = true;
        assert!(!scheduler.coalesces_alarms());
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(phase(&scheduler, process), Some(AlarmPhase::Active));
        assert_eq!(
            scheduler.blocked.timed_waiters.next_deadline(),
            Some(at(2_000_000))
        );
        // The overdue floor is off too.
        assert_eq!(
            scheduler.alarm_remaining(process, at(2_000_000)),
            LogicalTime::ZERO
        );
    }

    /// `getitimer` reads 0 for an Inactive timer and at least 1 us for an
    /// Active one whose deadline has passed; `setitimer`'s old value likewise.
    #[test]
    fn remaining_time_floors_an_overdue_timer_and_zeroes_an_inactive_one() {
        let (mut scheduler, process, leader) = one_process();
        assert_eq!(scheduler.alarm_remaining(process, at(1_000_000)), at(1_000));
        assert_eq!(scheduler.alarm_remaining(process, at(400_000)), at(600_000));
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            scheduler.alarm_remaining(process, at(1_000_000)),
            LogicalTime::ZERO
        );
        let (old, interval) = scheduler.register_alarm(
            process,
            leader,
            at(1_200_000),
            at(PERIOD),
            at(PERIOD),
            Signal::SIGALRM,
        );
        assert_eq!((old, interval), (LogicalTime::ZERO, at(PERIOD)));
    }

    /// Rule 4: a guest SIGALRM send into a process whose timer waits with no
    /// copy gives it one, so its dequeue re-arms the timer; an unclassifiable
    /// send makes every waiting process unobservable.
    #[test]
    fn a_guest_send_gives_a_waiting_timer_a_copy() {
        let (mut scheduler, process, leader) = one_process();
        set_state(&mut scheduler, leader, 0, 0, sigalrm_bit());
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: false })
        );
        // A handler is installed, then the guest sends SIGALRM to itself.
        set_state(&mut scheduler, leader, sigalrm_bit(), 0, 0);
        scheduler.note_guest_sigalrm(leader, GuestSigalrmTarget::Process(process));
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: true })
        );
        set_state(&mut scheduler, leader, 0, 0, 0);
        assert_eq!(
            scheduler.reconcile_alarm(process, at(2_300_000)),
            Some(at(3_000_000))
        );

        let (mut scheduler, process, leader) = one_process();
        pop_at(&mut scheduler, 1_000_000);
        scheduler.note_guest_sigalrm(leader, GuestSigalrmTarget::Unclassified);
        assert!(scheduler.alarm_unobservable.contains(&process));
    }

    /// Rule 5: setting SIG_IGN flushes the pending copy without a dequeue, so
    /// the timer stays inactive even though the queue is now empty.
    #[test]
    fn sig_ign_flushes_the_copy_without_re_arming() {
        let (mut scheduler, process, leader) = one_process();
        pop_at(&mut scheduler, 1_000_000);
        set_state(&mut scheduler, leader, 0, 0, sigalrm_bit());
        scheduler.note_sigalrm_disposition(leader);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: false })
        );
        assert_eq!(scheduler.reconcile_alarm(process, at(5_000_000)), None);
    }

    /// An expiry that pops during an exec barrier is decided when the barrier
    /// ends, with the committed time there, on the original grid.
    #[test]
    fn an_expiry_during_exec_waits_for_the_barrier() {
        let (mut scheduler, process, leader) = one_process();
        scheduler.exec_teardowns.insert(
            process,
            super::super::ExecTeardown {
                caller: leader,
                mm: crate::types::MmId::initial(process),
                siblings: Default::default(),
                observed: Default::default(),
            },
        );
        let attempts = scheduler.host_signal_attempts;
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            scheduler.host_signal_attempts, attempts,
            "nothing sent during exec"
        );
        assert!(scheduler.alarm_exec_deferred.contains_key(&process));
        scheduler.exec_teardowns.remove(&process);
        // The copy is dequeued before the barrier ends: the deferred send
        // queues one, and the deferred reconcile sees it still pending.
        set_state(&mut scheduler, leader, sigalrm_bit(), 0, 0);
        scheduler.committed_time = at(1_400_000);
        scheduler.finish_exec_alarm(process);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: true })
        );
        set_state(&mut scheduler, leader, 0, 0, 0);
        assert_eq!(
            scheduler.reconcile_alarm(process, at(1_500_000)),
            Some(at(2_000_000))
        );
        let _ = SignalTarget::Process(leader);
    }

    /// A process whose shared queue cannot be read while it has a live thread
    /// is refused by name; one whose exit has committed loses its timer.
    #[test]
    fn an_unreadable_queue_refuses_while_live_and_removes_after_exit() {
        let (mut scheduler, process, leader) = one_process();
        pop_at(&mut scheduler, 1_000_000);
        scheduler.test_kernel_signal_states.remove(&leader);
        assert_eq!(
            scheduler.shared_copy_outcome(process),
            SharedCopy::Unreadable
        );
        scheduler.next_turns.remove(&leader);
        assert_eq!(
            scheduler.shared_copy_outcome(process),
            SharedCopy::ProcessGone
        );
    }

    /// A private-queue dequeue leaves the shared copy in place, so nothing
    /// re-arms until the shared copy goes; a repeated reconcile at the same
    /// boundary (a controlled refresh that bumps with SkipTurn) is idempotent.
    #[test]
    fn a_private_dequeue_does_not_re_arm_and_reconcile_is_idempotent() {
        let (mut scheduler, process, leader) = one_process();
        pop_at(&mut scheduler, 1_000_000);
        // The private copy went; the shared one is still queued.
        set_state(&mut scheduler, leader, sigalrm_bit(), 0, 0);
        assert_eq!(scheduler.reconcile_alarm(process, at(1_200_000)), None);
        assert_eq!(scheduler.reconcile_alarm(process, at(1_200_000)), None);
        set_state(&mut scheduler, leader, 0, 0, 0);
        assert_eq!(
            scheduler.reconcile_alarm(process, at(1_300_000)),
            Some(at(2_000_000))
        );
        assert_eq!(scheduler.reconcile_alarm(process, at(1_300_000)), None);
        assert_eq!(scheduler.blocked.timed_waiters.iter().count(), 1);
    }

    /// Generation discard uses the guest's own mask while Detcore holds the
    /// thread under a private one (`saved_guest_sigmasks`), as Linux's
    /// `blocked | real_blocked` does around `rt_sigtimedwait`.
    #[test]
    fn generation_discard_reads_the_guests_own_mask() {
        let (mut scheduler, process, leader) = one_process();
        // The live SigBlk blocks SIGALRM (Detcore's private mask), but the
        // guest's own mask does not: an ignored expiry is discarded.
        set_state(&mut scheduler, leader, 0, sigalrm_bit(), sigalrm_bit());
        scheduler.saved_guest_sigmasks.insert(leader, 0);
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: false })
        );
    }

    /// An `F_SETSIG(SIGALRM)` armed by another process, whose descriptor
    /// owner may be any process, makes every process unobservable
    /// (container scope), as does a later change of owner.
    #[test]
    fn a_cross_process_f_setsig_makes_every_process_unobservable() {
        let (mut scheduler, process, _leader) = one_process();
        let other = DetTid::from_raw(300);
        scheduler.thread_tree.add_child(other, other, true);
        register_known_thread_for(&mut scheduler, other);
        assert!(scheduler.alarm_observable(process));
        scheduler.record_host_timed_signals(other, HostTimedSignalScope::Container, sigalrm_bit());
        assert!(!scheduler.alarm_observable(process));
    }

    /// A `CLONE_PARENT` child takes its creator's exit signal (Linux sets
    /// `exit_signal = current->group_leader->exit_signal`, whatever the clone
    /// asked for) and notifies the creator's parent. With the creator itself
    /// created with SIGALRM as its exit signal, the creator's parent stops
    /// being observable and the creator, which the child never notifies, does
    /// not.
    #[test]
    fn a_clone_parent_exit_signal_makes_the_creators_parent_unobservable() {
        let (mut scheduler, grandparent, gp_leader) = one_process();
        let creator = DetTid::from_raw(200);
        let child = DetTid::from_raw(300);
        scheduler.thread_tree.add_child_with_wait_metadata(
            gp_leader,
            creator,
            true,
            false,
            libc::SIGALRM,
        );
        scheduler.thread_tree.add_child_with_wait_metadata(
            creator,
            child,
            true,
            true,
            libc::SIGCHLD,
        );
        assert_eq!(
            scheduler
                .thread_tree
                .notification_signal(DetPid::from_raw(300)),
            Some(libc::SIGALRM),
            "the CLONE_PARENT child takes its creator's exit signal"
        );
        for thread in [creator, child] {
            register_known_thread_for(&mut scheduler, thread);
        }
        let creator_process = DetPid::from_raw(200);
        assert!(scheduler.alarm_observable(creator_process));
        assert!(!scheduler.alarm_observable(grandparent));
    }

    /// Rule 4 for a scheduler send: a SIGALRM `SIGEV_SIGNAL` POSIX timer's
    /// expiry gives a copy-less Inactive ITIMER_REAL its copy.
    #[test]
    fn a_posix_timer_sigalrm_gives_a_waiting_itimer_a_copy() {
        let (mut scheduler, process, leader) = one_process();
        set_state(&mut scheduler, leader, 0, 0, sigalrm_bit());
        pop_at(&mut scheduler, 1_000_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: false })
        );
        set_state(&mut scheduler, leader, sigalrm_bit(), 0, 0);
        scheduler.register_posix_timer(
            process,
            SignalTarget::Process(leader),
            7,
            Some(at(1_500_000)),
            LogicalTime::ZERO,
            Signal::SIGALRM.into(),
        );
        pop_at(&mut scheduler, 1_500_000);
        assert_eq!(
            phase(&scheduler, process),
            Some(AlarmPhase::Inactive { copy: true })
        );
    }
}
