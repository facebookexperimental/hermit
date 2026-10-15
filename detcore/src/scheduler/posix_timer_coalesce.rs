/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Linux's coalescing of POSIX interval timers, with overruns, on the traced
//! path (<https://github.com/rrnewton/hermit/issues/4262>).
//!
//! Linux queues a POSIX timer's expiry as the timer's own preallocated entry,
//! beside any ordinary copy of the same standard signal, and stops a periodic
//! timer until that entry is dequeued (`posix_timer_fn` returns
//! `HRTIMER_NORESTART`). The dequeue re-arms it on its grid
//! (`hrtimer_forward_now`) and reports the missed periods as `si_overrun`; an
//! entry whose timer was set again since it was queued is dropped inside the
//! dequeue. Before this, the traced path re-armed every expiry at the old
//! deadline plus one interval, so a slice that crossed N periods fired N
//! signals in a burst, and each send merged into the one pending copy.
//!
//! In an observable (process, signal) pair (`signal_observable`) the
//! scheduler keeps the entries Linux would hold (`signal_lists`), sends only
//! the front of each list, and at each dequeue it sees (a reported delivery
//! stop, a take, an unreported pass) pops the front, re-arms its timer and
//! sends the next one (promotion). Every other pair takes the fallback: each
//! expiry is sent at its pop with the overrun counted then, and the timer
//! forwards to its next grid point.
//!
//! Record and replay keep the legacy recurrence (`coalesces_alarms`).

use nix::sys::signal::Signal;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::info;

use super::Scheduler;
use super::discarded_at_send::Consumption;
use super::discarded_at_send::QueueId;
use super::discarded_at_send::SendFate;
use super::discarded_at_send::SignalProducer;
use super::exit_on_scheduler_refusal;
use super::kernel_signal_bit;
use super::signal_lists::Entry;
use super::signal_lists::FrontClass;
use super::signal_lists::KernelSiginfo;
use super::signal_lists::TimerAppend;
use super::signal_lists::TimerEntry;
use super::signal_lists::timer_siginfo;
use super::timed_waiters::AlarmPhase;
use super::timed_waiters::SignalTarget;
use super::timed_waiters::SignalTimerId;
use super::timed_waiters::grid_after;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// The signals whose pairs are never observable (4262 design §3.4): those the
/// kernel raises itself, the job-control signals, which any sender's stop or
/// SIGCONT flushes, Reverie's timer signal, and SIGKILL.
const EXCLUDED_SIGNALS: [i32; 15] = [
    libc::SIGPIPE,
    libc::SIGXFSZ,
    libc::SIGSYS,
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    libc::SIGFPE,
    libc::SIGTRAP,
    libc::SIGCONT,
    libc::SIGSTOP,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGSTKFLT,
    libc::SIGKILL,
];

/// What a reported delivery stop does with the signal it dequeued
/// (`Scheduler::timer_signal_dequeue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryAnswer {
    /// Deliver the physical record as it is.
    AsIs,
    /// Deliver it with this siginfo (`Guest::set_signal_info`).
    Rewrite(KernelSiginfo),
    /// Linux skips this entry inside the dequeue: deliver nothing.
    Suppress,
}

/// What a take (`rt_sigtimedwait`, a signalfd record) does with the copy it
/// took (`Scheduler::timer_signal_take`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeOutcome {
    /// Drop it and take again, as if it had never been queued: a copy Linux
    /// discarded when it was sent, or a stale timer entry Linux skips inside
    /// the dequeue.
    pub skip: bool,
    /// Return it with this siginfo instead of the physical record's.
    pub rewrite: Option<KernelSiginfo>,
}

/// A dequeue identified at a reported stop, popped once the stop's hooks
/// have read the queues (`Scheduler::finish_timer_signal_dequeue`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingDequeue {
    queue: QueueId,
    signal: i32,
    /// A delivery, after which the receiver runs the signal's handler, if
    /// it has one, with that handler's mask (not a take).
    delivery: bool,
    /// For a valid timer entry: the timer, the consuming thread's time, and
    /// the overrun its delivery reports.
    reconcile: Option<(i32, LogicalTime, i32)>,
}

/// The overrun Linux reports for a periodic timer last expired at `last`
/// with `interval`, dequeued at `t`: `hrtimer_forward_now` moves the expiry
/// past `t` by k intervals, and `it_overrun_last` is k - 1, clamped to
/// `INT_MAX` (`timer_overrun_to_int`).
pub(crate) fn overrun_at(last: LogicalTime, interval: LogicalTime, t: LogicalTime) -> i32 {
    let interval_ns = interval.as_nanos().max(1);
    // k - 1, the whole intervals elapsed since `last`.
    let missed = t.as_nanos().saturating_sub(last.as_nanos()) / interval_ns;
    i32::try_from(missed).unwrap_or(i32::MAX)
}

impl Scheduler {
    /// Record what `timer_settime` or `timer_delete` of `process`'s timer
    /// `timer` says: its `si_value`, a set's reset of the overrun
    /// (`do_timer_settime`), and that `process` has a timer on `signo`
    /// (`DiscardedAtSend::note_timer_history`). A delete removes the timer's
    /// park (`timer_delete`), and its value, which marks it deleted.
    pub(crate) fn note_posix_timer_notification(
        &mut self,
        process: DetPid,
        timer: i32,
        signo: i32,
        value: u64,
        deleted: bool,
    ) {
        if deleted {
            self.posix_timer_values.remove(&(process, timer));
            self.posix_timer_overruns.remove(&(process, timer));
            self.discarded_at_send.forget_park(process, timer);
            return;
        }
        self.posix_timer_values.insert((process, timer), value);
        self.posix_timer_overruns.insert((process, timer), 0);
        super::TIMER_COPIES_RECORDED.store(true, std::sync::atomic::Ordering::SeqCst);
        if (1..32).contains(&signo) {
            self.discarded_at_send.note_timer_history(process, signo);
        }
    }

    /// A successful exec of `process` deleted its POSIX timers (`exit_itimers`).
    pub(crate) fn delete_exec_posix_timers(&mut self, process: DetPid) {
        self.blocked.timed_waiters.remove_posix_timers(process);
        self.posix_timer_values
            .retain(|(owner, _), _| *owner != process);
        self.posix_timer_overruns
            .retain(|(owner, _), _| *owner != process);
        // Exec resets every caught signal's action (`flush_signal_handlers`).
        self.handler_masks.retain(|(owner, _), _| *owner != process);
    }

    /// Whether `process`'s timer `timer` is live (created and not deleted).
    fn posix_timer_live(&self, process: DetPid, timer: i32) -> bool {
        self.posix_timer_values.contains_key(&(process, timer))
    }

    /// The entry an expiry of `process`'s timer `timer` would queue now: its
    /// current generation and periodicity.
    pub(super) fn timer_entry_now(&self, process: DetPid, timer: i32) -> TimerEntry {
        TimerEntry {
            process,
            timer,
            gen_at_send: self.posix_timer_arm_generation(process, timer),
            periodic_at_send: self
                .blocked
                .timed_waiters
                .signal_timer_period(SignalTimerId::Posix(process, timer))
                .is_some(),
        }
    }

    /// Whether `entry` is current: its timer was not set or deleted since it
    /// was sent (Linux's `it_signal_seq == it_sigqueue_seq`).
    fn timer_entry_valid(&self, entry: &TimerEntry) -> bool {
        self.posix_timer_live(entry.process, entry.timer)
            && entry.gen_at_send == self.posix_timer_arm_generation(entry.process, entry.timer)
    }

    /// Whether the scheduler sees every dequeue of `signal` in `process` at a
    /// committed point (4262 design §3.4). Sticky once false; the first
    /// false answer is a transition (`lose_timer_observability`).
    pub(crate) fn signal_observable(&mut self, process: DetPid, signal: i32) -> bool {
        if !self.coalesces_alarms()
            || !(1..32).contains(&signal)
            || EXCLUDED_SIGNALS.contains(&signal)
        {
            return false;
        }
        if self.signal_unobservable.contains(&(process, signal)) {
            return false;
        }
        let bit = kernel_signal_bit(signal);
        let threads = self.process_signal_targets(process);
        let leader = DetTid::from_raw(process.as_raw());
        let host_timed = threads
            .iter()
            .any(|thread| self.host_timed_signals(*thread) & bit != 0);
        let child_exit = self
            .thread_tree
            .unreaped_children_of(process)
            .into_iter()
            .filter_map(|child| self.thread_tree.notification_signal(child))
            .any(|exit_signal| exit_signal == signal);
        let real_timer =
            signal == libc::SIGALRM && self.blocked.timed_waiters.alarm_phase(process).is_some();
        let observable = !self.background_call_processes.contains(&process)
            && !host_timed
            && !child_exit
            && !real_timer
            && threads.contains(&leader);
        if !observable {
            self.lose_timer_observability(process, signal);
        }
        observable
    }

    /// `signal` of `process` stops being observable (4262 design §4.11). A
    /// list holding a single valid entry that is its own physical record (or
    /// a `Plain` one that is not a `Proxy`), with no current park of the
    /// signal, falls back: the lists go, and the physical record is delivered
    /// as it stands. Anything else is refused by name, before any producer
    /// the scheduler cannot see runs.
    pub(crate) fn lose_timer_observability(&mut self, process: DetPid, signal: i32) {
        if !self.signal_unobservable.insert((process, signal)) {
            return;
        }
        debug!(
            "[dpid {}] signal {}: timer entries are not observable from here on; expiries take the fallback.",
            process, signal
        );
        for queue in self.process_queues(process) {
            let entries = self.discarded_at_send.lists.entries(queue, signal);
            let Some((front, class, _)) = self.discarded_at_send.lists.front(queue, signal) else {
                continue;
            };
            let single_own = entries.len() == 1
                && class == FrontClass::Own
                && match front {
                    Entry::Plain { .. } => true,
                    Entry::Timer(timer) => self.timer_entry_valid(&timer),
                };
            if !single_own {
                exit_on_scheduler_refusal(&format_args!(
                    "hermit: signal {signal} of process {process} stopped being observable (a call outside the schedule, a host-timed source, or a child's exit signal) while its pending entries in {queue:?} were {entries:?} with a {class:?} front: Linux would deliver them one by one, which the fallback cannot, so the run is refused (https://github.com/rrnewton/hermit/issues/4262)"
                ));
            }
            self.discarded_at_send.lists.take_all(queue, signal);
        }
        if self
            .discarded_at_send
            .parked_timers(process, signal)
            .iter()
            .any(|producer| self.posix_timer_expiry_current(producer))
        {
            exit_on_scheduler_refusal(&format_args!(
                "hermit: signal {signal} of process {process} stopped being observable while a POSIX timer's ignored expiry of it is parked: Linux re-queues it when the signal stops being ignored, which the fallback cannot follow, so the run is refused (https://github.com/rrnewton/hermit/issues/4262)"
            ));
        }
    }

    /// The processes with timer-signal entries or parked expiries.
    pub(crate) fn processes_with_timer_entries(&self) -> Vec<DetPid> {
        let mut processes: Vec<DetPid> = self
            .discarded_at_send
            .lists
            .listed()
            .into_iter()
            .map(|(queue, _)| match queue {
                QueueId::Process(process) => process,
                QueueId::Thread(thread) => self.sigchld_process(thread),
            })
            .chain(self.discarded_at_send.parked_processes())
            .collect();
        processes.sort();
        processes.dedup();
        processes
    }

    /// `child` was created: its exit signal, posted to its wait parent when it
    /// exits, ends that pair's observability (`signal_observable`).
    pub(crate) fn child_ends_timer_observability(&mut self, child: DetPid) {
        // A forked child copies its creator's actions.
        if let Some(parent) = self.thread_tree.parent_process(&child) {
            let inherited: Vec<_> = self
                .handler_masks
                .iter()
                .filter(|((owner, _), _)| *owner == parent)
                .map(|((_, signal), action)| (*signal, *action))
                .collect();
            for (signal, action) in inherited {
                self.handler_masks.insert((child, signal), action);
            }
        }
        if !self.coalesces_alarms() {
            return;
        }
        let (Some(parent), Some(signal)) = (
            self.thread_tree.parent_process(&child),
            self.thread_tree.notification_signal(child),
        ) else {
            return;
        };
        if (1..32).contains(&signal) {
            self.lose_timer_observability(parent, signal);
        }
    }

    /// A thread of `process` is granted a call outside the schedule: every
    /// signal of it stops being observable (`signal_observable` reads
    /// `background_call_processes`), and the transition is decided now,
    /// before the call can run.
    pub(crate) fn background_call_ends_timer_observability(&mut self, process: DetPid) {
        if !self.coalesces_alarms() {
            return;
        }
        for signal in 1..32 {
            self.lose_timer_observability(process, signal);
        }
    }

    /// Fire the expiry at `deadline` of POSIX timer `id`, notifying `target`
    /// with `signal`, which `pop_if_before` has just popped (and, if
    /// periodic, re-armed one interval later). Replaces that recurrence.
    pub(super) fn fire_posix_timer(
        &mut self,
        deadline: LogicalTime,
        id: SignalTimerId,
        target: SignalTarget,
        signal: Signal,
    ) {
        let SignalTimerId::Posix(process, timer) = id else {
            unreachable!("fire_posix_timer takes a POSIX timer");
        };
        let signo = signal as libc::c_int;
        if !self.models_signal_targets {
            // Elsewhere the scheduler cannot single out a thread's queue.
            match target {
                SignalTarget::Thread(thread) => self.fire_thread_timer(id, thread, signal),
                SignalTarget::Process(tid) => {
                    let producer = self.posix_timer_producer(id);
                    self.fire_process_timer(process, tid, signal, producer, None);
                }
            }
            return;
        }
        let now = self.committed_time;
        if !self.coalesces_alarms() || !self.signal_observable(process, signo) {
            self.fire_posix_timer_fallback(deadline, id, target, signal, now);
            return;
        }
        if let SignalTarget::Thread(thread) = target
            && !self.process_signal_targets(process).contains(&thread)
        {
            // Its recurrence stands: each period pops and is dropped
            // (disclosure 5).
            info!(
                "[dpid {}] Timer expired after its thread {} exited; dropped.",
                process, thread
            );
            return;
        }
        let periodic = self.blocked.timed_waiters.signal_timer_period(id).is_some();
        // A periodic timer waits for the dequeue of its entry, or of its
        // parked expiry's re-queued entry (`HRTIMER_NORESTART`).
        if periodic {
            self.blocked
                .timed_waiters
                .signal_timer_after_fire(id, deadline, Some(true), now);
        }
        let entry = self.timer_entry_now(process, timer);
        let (queue, named, preferred) = match target {
            SignalTarget::Thread(thread) => (QueueId::Thread(thread), thread, thread),
            SignalTarget::Process(tid) => (
                QueueId::Process(process),
                DetTid::from_raw(process.as_raw()),
                tid,
            ),
        };
        // Every expiry first refreshes the timer's queued entry (Linux sets
        // `it_sigqueue_seq` and `it_sig_periodic` before ALREADY_PENDING).
        let was_deliverable = self.list_deliverable(queue, signo);
        if self.discarded_at_send.lists.refresh_timer(entry) {
            info!(
                "[dpid {}] POSIX timer {} expired with its entry still queued; refreshed.",
                process, timer
            );
            if !was_deliverable && self.list_deliverable(queue, signo) {
                self.wake_for_entry(queue, signal, preferred);
            }
            return;
        }
        if self.discarded_when_sent_to(named, signo, None) {
            // Ignored: a periodic timer parks (4351's record keeps it, and the
            // copy, which carries `SI_TIMER`); a one-shot is dropped.
            let producer = periodic.then(|| self.posix_timer_producer(id)).flatten();
            if producer.is_none() {
                self.discarded_at_send.forget_park(process, timer);
            }
            let info = self.timer_signal_info(signo, &entry, 0);
            self.send_timer_record(
                queue,
                signal,
                preferred,
                info,
                SendFate::Classify(producer.map(SignalProducer::Posix)),
            );
            self.discarded_at_send.note_discarded_si_timer(
                queue,
                signo,
                super::signal_lists::SI_TIMER,
            );
            return;
        }
        // A parked timer whose signal is now blocked goes back to the queue.
        self.discarded_at_send.forget_park(process, timer);
        let standing = self
            .discarded_at_send
            .is_discarded(queue, signo)
            .then(|| self.discarded_at_send.discarded_is_si_timer(queue, signo));
        match self
            .discarded_at_send
            .lists
            .append_timer(queue, signo, entry, standing)
        {
            TimerAppend::SendFront | TimerAppend::ProxyFront => {
                let info = self.timer_signal_info(signo, &entry, 0);
                self.send_timer_record(queue, signal, preferred, info, SendFate::Kept);
            }
            TimerAppend::Behind => {
                info!(
                    "[dpid {}] POSIX timer {} expired behind another entry of signal {}; queued.",
                    process, timer, signal
                );
                if !was_deliverable {
                    self.wake_for_entry(queue, signal, preferred);
                }
            }
            TimerAppend::AlreadyPending => unreachable!("refreshed above"),
        }
    }

    /// The fallback (4262 design §4.12): send the expiry at the pop, with
    /// the overrun counted then, and forward the timer to its first grid
    /// point after the pop.
    fn fire_posix_timer_fallback(
        &mut self,
        deadline: LogicalTime,
        id: SignalTimerId,
        target: SignalTarget,
        signal: Signal,
        now: LogicalTime,
    ) {
        let SignalTimerId::Posix(process, timer) = id else {
            return;
        };
        let overrun = match self.blocked.timed_waiters.signal_timer_period(id) {
            Some(interval) if self.coalesces_alarms() => {
                self.blocked
                    .timed_waiters
                    .signal_timer_after_fire(id, deadline, None, now);
                overrun_at(deadline, interval, now)
            }
            _ => 0,
        };
        if self.coalesces_alarms() {
            self.posix_timer_overruns.insert((process, timer), overrun);
        }
        let entry = self.timer_entry_now(process, timer);
        let info = self.timer_signal_info(signal as libc::c_int, &entry, overrun);
        match target {
            SignalTarget::Thread(thread) => self.fire_thread_timer(id, thread, signal),
            SignalTarget::Process(tid) => {
                let producer = self.posix_timer_producer(id);
                self.fire_process_timer(process, tid, signal, producer, Some(info));
            }
        }
    }

    /// The `SI_TIMER` siginfo of `entry`'s delivery as `signal` with
    /// `overrun`, as a 128-byte `siginfo_t`.
    pub(crate) fn timer_signal_info(
        &self,
        signal: i32,
        entry: &TimerEntry,
        overrun: i32,
    ) -> [u8; 128] {
        let value = self
            .posix_timer_values
            .get(&(entry.process, entry.timer))
            .copied()
            .unwrap_or(0);
        timer_siginfo(signal, entry.timer, overrun, value).to_user()
    }

    /// Send a timer entry's physical record `info` into `queue` as `signal`:
    /// a shared-queue send goes through `fire_process_alarm`, the one body
    /// that keeps the shared-queue bookkeeping, preferring `preferred`; a
    /// private one through `send_guest_signal`'s `rt_tgsigqueueinfo`.
    fn send_timer_record(
        &mut self,
        queue: QueueId,
        signal: Signal,
        preferred: DetTid,
        info: [u8; 128],
        fate: SendFate,
    ) -> bool {
        match queue {
            QueueId::Process(process) => {
                self.guest_send_landed = None;
                let sent = self.fire_process_alarm(process, preferred, signal, Some(info), fate);
                // Rule 4: a scheduler send of SIGALRM into the shared queue
                // gives a copy-less Inactive ITIMER_REAL its copy
                // (https://github.com/rrnewton/hermit/issues/4139).
                if sent && signal == Signal::SIGALRM && self.guest_send_landed != Some(false) {
                    self.note_guest_sigalrm(
                        preferred,
                        super::itimer_coalesce::GuestSigalrmTarget::Process(process),
                    );
                }
                sent
            }
            QueueId::Thread(thread) => {
                let process = self.sigchld_process(thread);
                #[cfg(test)]
                {
                    self.host_signal_attempts += 1;
                }
                self.send_guest_signal(thread, signal, Some(process), Some(info), thread, fate);
                true
            }
        }
    }

    /// Whether (queue, signal) holds an entry a dequeue would deliver, or no
    /// list at all (`SignalLists::deliverable`).
    pub(super) fn list_deliverable(&self, queue: QueueId, signal: i32) -> bool {
        self.discarded_at_send
            .lists
            .deliverable(queue, signal, |process, timer| {
                if self.posix_timer_live(process, timer) {
                    self.posix_timer_arm_generation(process, timer)
                } else {
                    u64::MAX
                }
            })
    }

    /// The single-target wake of a kept send of `signal` into `queue` that
    /// needs no physical send (4262 design §4.13, rev 7 F): a timer entry
    /// appended behind a stale head, or a stale entry refreshed in place.
    /// The caller of an unignore (`preferred`) is running, so it is not woken.
    pub(super) fn wake_for_entry(&mut self, queue: QueueId, signal: Signal, preferred: DetTid) {
        let target = match queue {
            QueueId::Thread(thread) => Some(thread),
            QueueId::Process(process) => self
                .select_signal_target(process, Some(preferred))
                .map(|preferred| self.timer_signal_target(process, preferred, signal)),
        };
        let Some(target) = target else {
            return;
        };
        if let QueueId::Process(process) = queue
            && self
                .kernel_signal_target(process, target, signal as libc::c_int)
                .is_none()
        {
            self.note_signal_left_in_shared_queue(process, signal as libc::c_int);
        }
        if self.holds_serial_grant(target) {
            return;
        }
        self.wake_signaled_guest_as(target, signal, false);
    }

    /// At a reported delivery stop of `thread`, which dequeued `signal` at its
    /// logical time `at`: identify the entry it took and answer what to
    /// deliver (4262 design rev 8 A', §4.6 step 1). The signal is the one the
    /// stop reports; the queue comes from the model, private first. With no
    /// list for the signal in either queue, nothing is identified, and 4351's
    /// record decides as before.
    ///
    /// `at` is `None` for an unreported pass (4262 design §4.10), whose hook
    /// cannot replace the siginfo and has no thread time: a valid entry
    /// standing on its own record is delivered with that record (overrun 0,
    /// disclosed) and its timer reconciled at the committed time, and one
    /// standing on another sender's record is refused by name.
    pub(crate) fn timer_signal_dequeue(
        &mut self,
        thread: DetTid,
        signal: i32,
        at: Option<LogicalTime>,
    ) -> DeliveryAnswer {
        self.pending_timer_dequeues.remove(&thread);
        if !self.coalesces_alarms() || !(1..32).contains(&signal) {
            return DeliveryAnswer::AsIs;
        }
        let process = self.sigchld_process(thread);
        let Some(queue) = self.identify_listed_queue(thread, process, signal) else {
            return DeliveryAnswer::AsIs;
        };
        let (front, class, _) = self
            .discarded_at_send
            .lists
            .front(queue, signal)
            .expect("an identified queue has a list");
        let reported = at.is_some();
        let at = at.unwrap_or(self.committed_time);
        let stale = matches!(front, Entry::Timer(entry) if !self.timer_entry_valid(&entry));
        if !reported && class == FrontClass::Proxy && !stale {
            exit_on_scheduler_refusal(&format_args!(
                "hermit: thread {thread} took signal {signal} from {queue:?} where the backend passes it on unreported, so its siginfo cannot be replaced, and the pending record is another sender's standing for {front:?}: the guest would see that sender's siginfo, so the run is refused (https://github.com/rrnewton/hermit/issues/4262)"
            ));
        }
        let (answer, reconcile) = match front {
            Entry::Plain { siginfo, .. } => match class {
                FrontClass::Own => (DeliveryAnswer::AsIs, None),
                FrontClass::Proxy => (DeliveryAnswer::Rewrite(siginfo), None),
            },
            Entry::Timer(_) if stale => (DeliveryAnswer::Suppress, None),
            Entry::Timer(entry) => {
                let overrun = self.dequeue_overrun(&entry, at);
                let answer = if reported {
                    let info = self.timer_signal_info(signal, &entry, overrun);
                    DeliveryAnswer::Rewrite(KernelSiginfo::from_user(&info))
                } else {
                    DeliveryAnswer::AsIs
                };
                (answer, Some((entry.timer, at, overrun)))
            }
        };
        self.pending_timer_dequeues.insert(
            thread,
            PendingDequeue {
                queue,
                signal,
                delivery: true,
                reconcile,
            },
        );
        answer
    }

    /// A take of `signal` by `thread` at its time `at` (4262 design §4.4,
    /// rev 8 A'): the returned signal is authoritative. With no list for it
    /// in either queue, 4351's record decides as before (`take_from`).
    /// Otherwise the entry is identified from the model, private first,
    /// checked against the queues read after the take, where read (the
    /// identified queue's bit clear, the other queue's as its model says),
    /// popped, its timer re-armed, and the next entry promoted before the
    /// caller takes again.
    pub(crate) fn timer_signal_take(
        &mut self,
        thread: DetTid,
        signal: i32,
        at: LogicalTime,
        after: Option<(u64, u64)>,
    ) -> TakeOutcome {
        let process = self.sigchld_process(thread);
        let listed = if self.coalesces_alarms() && (1..32).contains(&signal) {
            self.identify_listed_queue(thread, process, signal)
        } else {
            None
        };
        let Some(queue) = listed else {
            return TakeOutcome {
                skip: self.discarded_at_send.take_from(thread, process, signal),
                rewrite: None,
            };
        };
        if let Some((thread_pending, shared_pending)) = after {
            let bit = kernel_signal_bit(signal);
            self.check_taken_queues(
                thread,
                queue,
                signal,
                thread_pending & bit != 0,
                shared_pending & bit != 0,
            );
        }
        let (front, class, _) = self
            .discarded_at_send
            .lists
            .front(queue, signal)
            .expect("an identified queue has a list");
        let (outcome, reconcile) = match front {
            Entry::Plain { siginfo, .. } => (
                TakeOutcome {
                    skip: false,
                    rewrite: (class == FrontClass::Proxy).then_some(siginfo),
                },
                None,
            ),
            Entry::Timer(entry) if !self.timer_entry_valid(&entry) => (
                TakeOutcome {
                    skip: true,
                    rewrite: None,
                },
                None,
            ),
            Entry::Timer(entry) => {
                let overrun = self.dequeue_overrun(&entry, at);
                let info = self.timer_signal_info(signal, &entry, overrun);
                (
                    TakeOutcome {
                        skip: false,
                        rewrite: Some(KernelSiginfo::from_user(&info)),
                    },
                    Some((entry.timer, at, overrun)),
                )
            }
        };
        self.discarded_at_send.take(queue, signal);
        self.pop_and_promote(
            PendingDequeue {
                queue,
                signal,
                delivery: false,
                reconcile,
            },
            thread,
        );
        outcome
    }

    /// A take for the signals `set` by `thread` at `at` failed copying out
    /// the record it dequeued, which the kernel does not name; afterwards the
    /// queues held `thread_pending` and `shared_pending` (rev 7 A3, rev 8 A',
    /// N8-3). A listed (queue, signal) of the set whose bit is now clear is
    /// the one taken: nothing else consumes in the caller's turn. With none,
    /// the take was of an unlisted signal, which 4351's record decides
    /// (`take_faulted`). Two such cannot be told apart, and refuse by name.
    /// A stale entry is popped and the take repeats, as Linux would have
    /// skipped it; a valid one is popped, re-armed and promoted, and the
    /// fault stands. Returns whether to take again.
    pub(crate) fn timer_signal_take_faulted(
        &mut self,
        thread: DetTid,
        set: u64,
        thread_pending: u64,
        shared_pending: u64,
        at: LogicalTime,
    ) -> bool {
        let process = self.sigchld_process(thread);
        let (private, shared) = (QueueId::Thread(thread), QueueId::Process(process));
        let mut taken = Vec::new();
        if self.coalesces_alarms() {
            for (queue, pending) in [(private, thread_pending), (shared, shared_pending)] {
                for signal in 1..32 {
                    let bit = kernel_signal_bit(signal);
                    if set & bit != 0
                        && pending & bit == 0
                        && self.discarded_at_send.lists.is_listed(queue, signal)
                    {
                        taken.push((queue, signal));
                    }
                }
            }
        }
        let (queue, signal) = match taken.as_slice() {
            [] => {
                return self.discarded_at_send.take_faulted(
                    thread,
                    process,
                    set,
                    thread_pending,
                    shared_pending,
                );
            }
            [one] => *one,
            _ => exit_on_scheduler_refusal(&format_args!(
                "hermit: thread {thread}'s rt_sigtimedwait failed copying out a signal it dequeued, and the entries of {taken:?} all left their queues, so which one Linux took cannot be told; the run is refused (https://github.com/rrnewton/hermit/issues/4262)"
            )),
        };
        let (front, _, _) = self
            .discarded_at_send
            .lists
            .front(queue, signal)
            .expect("a listed queue has a front");
        let (skip, reconcile) = match front {
            Entry::Timer(entry) if !self.timer_entry_valid(&entry) => (true, None),
            Entry::Timer(entry) => {
                let overrun = self.dequeue_overrun(&entry, at);
                (false, Some((entry.timer, at, overrun)))
            }
            Entry::Plain { .. } => (false, None),
        };
        self.discarded_at_send.take(queue, signal);
        self.pop_and_promote(
            PendingDequeue {
                queue,
                signal,
                delivery: false,
                reconcile,
            },
            thread,
        );
        skip
    }

    /// The listed queue a dequeue of `signal` by `thread` took from, private
    /// first (Linux's `dequeue_signal`), or `None` when neither queue has a
    /// list for it (the no-list branch) or the private queue's copy is a
    /// discarded one with no list (4351 decides).
    fn identify_listed_queue(
        &self,
        thread: DetTid,
        process: DetPid,
        signal: i32,
    ) -> Option<QueueId> {
        let (private, shared) = (QueueId::Thread(thread), QueueId::Process(process));
        let lists = &self.discarded_at_send.lists;
        if !lists.is_listed(private, signal) && !lists.is_listed(shared, signal) {
            return None;
        }
        let queue = if lists.is_listed(private, signal)
            || self.discarded_at_send.is_discarded(private, signal)
        {
            private
        } else {
            shared
        };
        lists.is_listed(queue, signal).then_some(queue)
    }

    /// The overrun a valid entry's delivery at `at` reports: for a periodic
    /// timer waiting for this dequeue, the missed periods since its last
    /// expiry; 0 for a one-shot.
    fn dequeue_overrun(&self, entry: &TimerEntry, at: LogicalTime) -> i32 {
        let id = SignalTimerId::Posix(entry.process, entry.timer);
        match self.blocked.timed_waiters.signal_timer_state(id) {
            Some((last, interval, AlarmPhase::Inactive { .. }))
                if entry.periodic_at_send && interval != LogicalTime::ZERO =>
            {
                overrun_at(last, interval, at)
            }
            _ => 0,
        }
    }

    /// After a reported stop's hooks read `thread`'s private queue
    /// (`thread_holds`) and its process's shared one (`process_holds`): pop
    /// the entry the stop took, re-arm its timer, and send the next entry
    /// (4262 design §4.6 steps 2 and 3). A copy resumed blocked is requeued
    /// by the kernel, so nothing pops. The read must agree with the model
    /// (rev 9 A''): the identified queue's bit is clear, since the promotion
    /// has not been sent yet, and the other queue's bit is set exactly when
    /// its list or its discarded bit says a copy is there.
    /// Returns the dequeue to pop (`pop_and_promote`), once 4351's record has
    /// retired its bits.
    pub(crate) fn finish_timer_signal_dequeue(
        &mut self,
        thread: DetTid,
        signal: i32,
        how: Consumption,
        thread_holds: bool,
        process_holds: bool,
    ) -> Option<PendingDequeue> {
        let pending = self.pending_timer_dequeues.remove(&thread)?;
        if pending.signal != signal || how == (Consumption::KeptResume { blocked: true }) {
            return None;
        }
        self.check_taken_queues(thread, pending.queue, signal, thread_holds, process_holds);
        Some(pending)
    }

    /// The check of a dequeue of `signal` from `queue` by `thread` against
    /// its queues read after it, before any promotion (rev 9 A''): the
    /// identified queue no longer holds the signal, and the other one holds
    /// it exactly when its list or its discarded bit says so. A
    /// contradiction is a producer the schedule does not see, and refuses by
    /// name.
    fn check_taken_queues(
        &self,
        thread: DetTid,
        queue: QueueId,
        signal: i32,
        thread_holds: bool,
        process_holds: bool,
    ) {
        let process = self.sigchld_process(thread);
        let (private, shared) = (QueueId::Thread(thread), QueueId::Process(process));
        let (other, identified_holds, other_holds) = if queue == private {
            (shared, thread_holds, process_holds)
        } else {
            (private, process_holds, thread_holds)
        };
        let other_expected = self.discarded_at_send.lists.is_listed(other, signal)
            || self.discarded_at_send.is_discarded(other, signal);
        if identified_holds || other_holds != other_expected {
            exit_on_scheduler_refusal(&format_args!(
                "hermit: thread {thread} took signal {signal} from {queue:?}, and afterwards that queue {} the signal and {other:?} {} it, where the model of its pending entries expects {}: a producer the schedule does not see, so the run is refused (https://github.com/rrnewton/hermit/issues/4262)",
                if identified_holds { "still held" } else { "no longer held" },
                if other_holds { "held" } else { "did not hold" },
                if other_expected {
                    "it held there"
                } else {
                    "it absent there"
                },
            ));
        }
    }

    /// Pop `pending`'s front, reconcile its timer, and promote the next entry
    /// with `receiver` the thread that took it.
    pub(crate) fn pop_and_promote(&mut self, pending: PendingDequeue, receiver: DetTid) {
        let Some(popped) = self
            .discarded_at_send
            .lists
            .pop(pending.queue, pending.signal)
        else {
            return;
        };
        if let (Entry::Timer(entry), Some((timer, at, overrun))) = (popped.entry, pending.reconcile)
        {
            debug_assert_eq!(entry.timer, timer);
            let id = SignalTimerId::Posix(entry.process, timer);
            if entry.periodic_at_send
                && let Some(next) = self.blocked.timed_waiters.signal_timer_rearm_after(id, at)
            {
                self.posix_timer_overruns
                    .insert((entry.process, timer), overrun);
                debug!(
                    "[dpid {}] POSIX timer {} delivered with overrun {}; re-armed at {}.",
                    entry.process, timer, overrun, next
                );
            }
        }
        if let Some(Entry::Timer(next)) = popped.next {
            // At a delivery the kernel sets up the handler frame before the
            // receiver can take anything else: its mask then decides the
            // promotion's target (§4.5, P3-3).
            let process = self.sigchld_process(receiver);
            self.delivery_mask_override = match self.handler_masks.get(&(process, pending.signal)) {
                Some(&(mask, nodefer)) if pending.delivery => {
                    let own = self.thread_signal_mask(receiver).unwrap_or(0);
                    let deferred = if nodefer {
                        0
                    } else {
                        kernel_signal_bit(pending.signal)
                    };
                    Some((receiver, own | mask | deferred))
                }
                _ => None,
            };
            self.promote(pending.queue, pending.signal, next, receiver);
            self.delivery_mask_override = None;
        }
    }

    /// Send the new front `next` of `queue`'s list for `signal` (promotion,
    /// 4262 design §4.5): kept, as Linux queued it at its own expiry.
    fn promote(&mut self, queue: QueueId, signal: i32, next: TimerEntry, receiver: DetTid) {
        let Ok(sig) = Signal::try_from(signal) else {
            return;
        };
        info!(
            "[dtid {}] signal {} dequeued from {:?}; promoting POSIX timer {}'s entry.",
            receiver, sig, queue, next.timer
        );
        let info = self.timer_signal_info(signal, &next, 0);
        self.send_timer_record(queue, sig, receiver, info, SendFate::Kept);
    }

    /// Thread `thread` exited, taking its private lists with it
    /// (`entries`, N9-5): a periodic timer that waited for the dequeue of one
    /// of them re-arms now, so its next expiry finds the thread gone and is
    /// dropped, as Linux drops it, with the timer still armed.
    pub(crate) fn note_private_entries_retired(&mut self, entries: Vec<Entry>) {
        let now = self.committed_time;
        for entry in entries {
            if let Entry::Timer(entry) = entry
                && entry.periodic_at_send
            {
                self.blocked.timed_waiters.signal_timer_rearm_after(
                    SignalTimerId::Posix(entry.process, entry.timer),
                    now,
                );
            }
        }
    }

    /// `timer_getoverrun` of `process`'s timer `timer`: `it_overrun_last`.
    pub(crate) fn posix_timer_overrun(&self, process: DetPid, timer: i32) -> i32 {
        self.posix_timer_overruns
            .get(&(process, timer))
            .copied()
            .unwrap_or(0)
    }

    /// `timer_gettime`'s remaining time of `process`'s timer `timer` at `now`
    /// (4262 design §5), or `None` where the legacy answer stands (record and
    /// replay). An Active timer past its deadline, whose pop waits for a turn
    /// boundary, reads 1 ns (`common_timer_get` of an armed hrtimer); one
    /// waiting for the dequeue of its entry reads the time to its next grid
    /// point, a full interval on an exact hit (`timer_forward`); a popped
    /// one-shot or a disarmed timer reads 0.
    pub(crate) fn posix_timer_remaining(
        &self,
        process: DetPid,
        timer: i32,
        now: LogicalTime,
    ) -> Option<LogicalTime> {
        if !self.coalesces_alarms() {
            return None;
        }
        let id = SignalTimerId::Posix(process, timer);
        let remaining = match self.blocked.timed_waiters.signal_timer_state(id) {
            None => 0,
            Some((deadline, _, AlarmPhase::Active)) => {
                deadline.as_nanos().saturating_sub(now.as_nanos()).max(1)
            }
            Some((last, interval, AlarmPhase::Inactive { .. })) => {
                grid_after(last, interval, now).as_nanos() - now.as_nanos()
            }
        };
        Some(LogicalTime::from_nanos(remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    #[test]
    fn the_overrun_counts_the_missed_periods() {
        // Expired at 10 with interval 10; dequeued before the next period.
        assert_eq!(overrun_at(at(10), at(10), at(15)), 0);
        // An exact hit on the next grid point forwards past it.
        assert_eq!(overrun_at(at(10), at(10), at(20)), 1);
        // Five periods blocked: 4 missed.
        assert_eq!(overrun_at(at(10), at(10), at(55)), 4);
        assert_eq!(overrun_at(at(0), at(1), at(u64::MAX)), i32::MAX);
    }
}
