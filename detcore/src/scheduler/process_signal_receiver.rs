/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The thread of a multi-threaded process that takes a process-directed
//! signal a guest sends (<https://github.com/rrnewton/hermit/issues/4034>).
//!
//! Linux queues such a signal on the process's shared queue
//! (`__send_signal_locked`) and then `complete_signal` picks the thread it
//! wakes to take it (kernel/signal.c:963-997 at v6.16):
//!
//! 1. the task the call names, `p` (the leader for a process id, the named
//!    thread for one of its thread ids), if it `wants_signal`;
//! 2. otherwise the first thread that does, walking the thread list from the
//!    process's `signal->curr_target`, which then becomes that thread;
//! 3. when no thread does, none: the signal waits in the shared queue, and the
//!    first thread that unblocks it, or that finishes the signals it already
//!    has pending, takes it.
//!
//! `wants_signal` (signal.c:946-961) asks that the thread not block the
//! signal, not be exiting (`PF_EXITING`) and not be stopped or traced, and
//! that it be running (`task_curr`) or have no signal pending
//! (`task_sigpending`).
//!
//! Under ptrace every thread the scheduler holds is traced, so the kernel's
//! own choice degenerates: `wants_signal` passes over every stopped thread,
//! and the signal waits in the shared queue for whichever thread resumes
//! first and does not block it (`ptrace_stop` ends with
//! `recalc_sigpending_tsk`, signal.c:2491-2496). So Detcore makes the choice, by
//! the same rule over its own model, and then keeps the call itself: it
//! forwards the guest's `kill`, `rt_sigqueueinfo` or `pidfd_send_signal`
//! unchanged, so the copy is the kernel's own, in the shared queue, with
//! Linux's siginfo, coalescing, retargeting and visibility; and it resumes
//! the chosen thread before any other that could take it, until it does
//! (`process_signal_takers`, `Scheduler::first_process_signal_taker`).
//!
//! The model, in which the guest's threads share one CPU and only the sender
//! runs:
//!
//! - The thread list is thread-ID order. `copy_process` appends each new
//!   thread at the list's tail (kernel/fork.c:2396-2397), and in a PID
//!   namespace `alloc_pid` hands out ids cyclically upward
//!   (`idr_alloc_cyclic`, kernel/pid.c:237), so a later thread has the higher
//!   id until the ids wrap.
//! - `curr_target` starts at the leader (kernel/fork.c:1662) and moves to each
//!   thread step 2 chooses, once the forwarded call has succeeded
//!   (`Scheduler::process_signal_sent`): a call the kernel fails before it
//!   queues anything, or one Detcore refuses, never reaches
//!   `complete_signal`. That includes a send to the sender's own process
//!   that Detcore forwards because Linux gives it to the sender
//!   (`Scheduler::own_process_signal_choice`): when the call names another
//!   thread, which blocks the signal, the walk is what reaches the sender.
//!   Linux moves it on when that thread exits
//!   (`__exit_signal`, kernel/exit.c:187-188); here the walk starts at the
//!   first live thread at or after it, which is the same thread unless a
//!   thread was created after the last thread in the list exited.
//! - A thread is exiting once its exit is granted, it was killed at a turn, or
//!   its exit waits for a delivery permit.
//! - `task_curr` holds for the sender alone, and SIGKILL is wanted whatever is
//!   pending (signal.c:954-955). A thread "has a signal pending" when its own
//!   queue holds a signal it does not block, other than one a host-timed
//!   source may have posted (`Scheduler::host_timed_signals`) or Reverie's
//!   preemption marker. A copy in the shared queue does not count: Linux sets
//!   `TIF_SIGPENDING` for it only on the thread it woke.
//!
//! Every input is the schedule's: the scheduler's committed records, and the
//! `/proc` masks and private queues of threads stopped at a request or
//! parked, which change only when they run, while the sender holds the turn.
//!
//! Two cases keep the fail-closed refusal they had before this choice existed
//! (<https://github.com/rrnewton/hermit/issues/4034>), because the forwarded
//! copy would not reach the chosen thread:
//!
//! - a thread sleeping outside the schedule (external I/O, a real
//!   `rt_sigsuspend`) is not traced, so the kernel can wake it for the signal
//!   at a moment host timing sets, whatever Detcore chose
//!   (`ProcessSignalReceiver::OutOfSchedule`);
//! - a thread that can take the copy outside its turn, whatever its own mask
//!   says: its filed call installs a temporary mask that admits the signal
//!   (`rt_sigsuspend`, `ppoll`, `pselect6`, `epoll_pwait`), a signalfd of the
//!   process covers it, or a thread waits for it in `rt_sigtimedwait`
//!   (`ProcessSignalReceiver::TakenOutOfTurn`);
//! - a send to the sender's own process that Linux gives to another thread:
//!   the sender is running, so `task_curr` makes it the one thread
//!   `wants_signal` admits, and it takes the copy at its return to user mode
//!   before any other thread runs (`ProcessSignalReceiver::SenderTakesIt`).
//!   The sends Linux gives to the sender itself are forwarded, here and by
//!   the earlier admission that holds the sender's turn
//!   (`Detcore::own_process_signal`).
//!
//! A signal whose default action ends the process is exempt from all three:
//! Linux ends the whole group with the same status whichever thread takes it
//! (`get_signal` calls `do_group_exit(signr)`, signal.c:3034), and no thread
//! runs further user code, so nothing the refusals protect is left to decide.
//! A guest's own abort or fault (`raise(SIGABRT)` after a handler resets the
//! disposition, a watchdog's `pidfd_send_signal(self, SIGSEGV)`) takes this
//! path. Which thread carries the death out, and so how far a thread
//! sleeping outside the schedule gets first, is the kernel's; the status and
//! the death itself are not.
//!
//! A standard signal the shared queue already holds is queued no second time
//! (`legacy_queue`, signal.c:1037-1040), so such a send reaches no
//! `complete_signal`: it chooses nothing, marks nothing, and moves
//! `curr_target` no further.
//!
//! Linux's group-exit shortcut at the send (signal.c:1003-1025) needs a
//! thread that wants the signal, which no ptrace-stopped thread does, so for
//! a fatal signal it is the receiver's delivery that ends the group, one turn
//! later than native; the exit status is the same.

use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use super::Scheduler;
use super::discarded_at_send::STANDARD_SIGNALS;
use super::kernel_signal_bit;
use crate::resources::ResourceID;
use crate::syscalls::KernelSignalState;
use crate::types::DetPid;
use crate::types::DetTid;

/// Which thread takes a process-directed signal
/// (`Scheduler::process_signal_receiver`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessSignalReceiver {
    /// The thread `complete_signal` gives the signal, which the scheduler
    /// resumes first until it takes the forwarded copy
    /// (`Scheduler::first_process_signal_taker`). `sole`: every other live
    /// thread blocks the signal or is exiting, so no other thread can take
    /// the copy before it anyway, and nothing is marked.
    Thread { thread: DetTid, sole: bool },
    /// No thread wants it: it waits in the process's shared queue.
    SharedQueue,
    /// This thread sleeps outside the schedule and does not block the
    /// signal, so the kernel can wake it for the copy at a host-timed moment.
    OutOfSchedule(DetTid),
    /// This thread can take the copy from the shared queue outside its turn,
    /// whatever its own mask says: its filed call installs a temporary mask
    /// that admits the signal, or it reads a signalfd that covers it, or a
    /// sibling waits for it in `rt_sigtimedwait`.
    TakenOutOfTurn(DetTid),
    /// The sender's own process, with the sender not blocking the signal and
    /// another thread the receiver: the sender is the one thread Linux's
    /// `wants_signal` admits (it is `task_curr`, and every other thread is
    /// ptrace-stopped), so it takes the forwarded copy at its return to user
    /// mode, before the receiver can run.
    SenderTakesIt,
    /// The choice depends on this thread, whose signal mask is unknown.
    UnknownMask(DetTid),
    /// The process's shared queue already holds the signal, so Linux queues
    /// no second copy and chooses no thread (`legacy_queue`).
    AlreadyQueued,
    /// An exec is in flight in the process; its `de_thread` decides which
    /// threads remain.
    ExecInFlight,
    /// This scheduler does not model signal targets (only ptrace does).
    NotModeled,
    /// A schedule replay chose `receiver`, as its recording did, but the
    /// recorded schedule next runs `first`, another thread of the process
    /// that could take the copy before it: the replay has left its recording.
    ScheduleRunsFirst { receiver: DetTid, first: DetTid },
}

/// Whether one thread `wants_signal`, in the model above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wants {
    Yes,
    /// It blocks the signal, or is exiting.
    No,
    /// It leaves the signal unblocked but already has one pending
    /// (`task_sigpending`), so Linux passes it over; it can still take the
    /// copy from the shared queue once it is done with those.
    Busy,
    /// Its mask is unknown.
    Unknown,
}

/// A live thread of the process to read its shared queue through: the
/// sender when it belongs to the process, else its first live thread.
fn sender_or_member(threads: &[(DetTid, Wants)], sender: DetTid) -> DetTid {
    if threads.iter().any(|(thread, _)| *thread == sender) {
        sender
    } else {
        threads.first().map_or(sender, |(thread, _)| *thread)
    }
}

/// `complete_signal`'s choice among `threads` (every live thread of the
/// process in list order, with whether each wants the signal), trying `named`
/// first and then walking from `curr_target`. Returns the receiver and, when
/// the walk chose it, the process's new `curr_target`.
///
/// When no thread wants it, Linux leaves it in the shared queue for the first
/// thread that can take it.
pub(crate) fn complete_signal(
    threads: &[(DetTid, Wants)],
    named: DetTid,
    curr_target: DetTid,
) -> (ProcessSignalReceiver, Option<DetTid>) {
    let chosen = |thread: DetTid| ProcessSignalReceiver::Thread {
        thread,
        sole: threads
            .iter()
            .all(|(tid, wants)| *tid == thread || *wants == Wants::No),
    };
    match threads.iter().find(|(tid, _)| *tid == named) {
        Some((_, Wants::Yes)) => return (chosen(named), None),
        Some((_, Wants::Unknown)) => return (ProcessSignalReceiver::UnknownMask(named), None),
        Some((_, Wants::No | Wants::Busy)) | None => {}
    }
    let start = threads
        .iter()
        .position(|(tid, _)| *tid >= curr_target)
        .unwrap_or(0);
    let walk = || threads[start..].iter().chain(&threads[..start]);
    for (tid, wants) in walk() {
        match wants {
            Wants::Yes => return (chosen(*tid), Some(*tid)),
            Wants::Unknown => return (ProcessSignalReceiver::UnknownMask(*tid), None),
            Wants::No | Wants::Busy => {}
        }
    }
    (ProcessSignalReceiver::SharedQueue, None)
}

impl Scheduler {
    /// The thread of `process` that takes the process-directed signal
    /// `signo`, which `sender` sends naming `named` (module docs), decided in
    /// `sender`'s turn. A chosen thread found by the walk becomes the
    /// process's `curr_target`, and one that siblings could take the copy
    /// before is marked as its taker (`process_signal_takers`).
    pub(crate) fn process_signal_receiver(
        &mut self,
        process: DetPid,
        named: DetTid,
        sender: DetTid,
        signo: i32,
    ) -> ProcessSignalReceiver {
        // Only this choice's own move can be committed by the call that
        // follows it.
        self.pending_signal_curr_target = None;
        if !self.models_signal_targets {
            return ProcessSignalReceiver::NotModeled;
        }
        if self.exec_teardowns.contains_key(&process) {
            return ProcessSignalReceiver::ExecInFlight;
        }
        let bit = kernel_signal_bit(signo);
        let threads: Vec<(DetTid, Wants)> = self
            .process_signal_targets(process)
            .into_iter()
            .map(|thread| (thread, self.wants_signal(thread, sender, bit)))
            .collect();
        // A signal whose default action ends the process needs no choice at
        // all: whichever thread takes it, Linux ends the whole thread group
        // with the same status (`get_signal` calls `do_group_exit(signr)`,
        // signal.c:3034; for a core-dumping signal `do_coredump` kills the
        // others first, signal.c:3010-3020), and no thread runs further user
        // code. So the two refusals below, which are about WHICH thread takes
        // the copy, do not apply to it; the kernel's own choice under ptrace
        // is as good as Linux's. The aborts and faults a guest raises in its
        // own multi-threaded process take this path.
        if threads
            .first()
            .and_then(|(thread, _)| self.stopped_thread_signal_state(*thread).ok())
            .is_some_and(|state| state.default_action_ends_process(signo))
        {
            // Every thread blocks it: Linux leaves it in the shared queue, and
            // no thread takes it until one unblocks it.
            let Some(thread) = threads
                .iter()
                .find(|(_, wants)| *wants != Wants::No)
                .map(|(thread, _)| *thread)
            else {
                tracing::info!(
                    "[scheduler] process-directed signal {} from {} to process {} would end its \
                     thread group, but every thread blocks it: it waits in the shared queue",
                    signo,
                    sender,
                    process,
                );
                return ProcessSignalReceiver::SharedQueue;
            };
            tracing::info!(
                "[scheduler] process-directed signal {} from {} to process {} ends its thread \
                 group whichever thread takes it; thread {} is named",
                signo,
                sender,
                process,
                thread,
            );
            return ProcessSignalReceiver::Thread { thread, sole: true };
        }
        // The kernel wakes such a thread itself, at a host-timed moment, so
        // which thread takes the copy is not the schedule's.
        if let Some((thread, _)) = threads
            .iter()
            .find(|(thread, wants)| *wants != Wants::No && self.out_of_schedule(*thread))
        {
            return ProcessSignalReceiver::OutOfSchedule(*thread);
        }
        // A thread whose turn is not where it takes the copy: its call's
        // temporary mask admits the signal although its own mask blocks it, a
        // signalfd it reads in the background covers it, or a sibling's
        // `rt_sigtimedwait` set does. Each takes the copy at a moment host
        // timing sets, as it does for the scheduler's own sends
        // (`another_call_may_take_waiting_signal`), so no choice here holds.
        if let Some(thread) = self.may_take_out_of_turn(process, bit) {
            return ProcessSignalReceiver::TakenOutOfTurn(thread);
        }
        // Linux does not queue a standard signal the shared queue already
        // holds (`legacy_queue`, signal.c:1037-1040 through
        // `__send_signal_locked`, signal.c:1063-1064), so the send reaches no
        // `complete_signal`: the thread chosen for the copy already there
        // keeps it, and this send decides nothing.
        if bit & STANDARD_SIGNALS != 0
            && self
                .read_shared_pending(sender_or_member(&threads, sender))
                .is_some_and(|pending| pending & bit != 0)
        {
            tracing::info!(
                "[scheduler] process-directed signal {} from {} to process {} is already in its \
                 shared queue; Linux queues no second copy",
                signo,
                sender,
                process,
            );
            return ProcessSignalReceiver::AlreadyQueued;
        }
        let curr_target = self.signal_curr_target(process);
        let (receiver, moved) = complete_signal(&threads, named, curr_target);
        // A schedule replay decides as its recording did, from the same
        // schedule, and resumes the threads in the recorded order, so the
        // chosen thread, which the recording resumed before any other that
        // could take the copy (`first_process_signal_taker`), is the next of
        // them its schedule runs. If it is not, the replay has left its
        // recording, and another thread could take the copy first.
        if let ProcessSignalReceiver::Thread {
            thread,
            sole: false,
        } = receiver
            && thread != sender
            && let Some(first) = self.replayer.as_ref().and_then(|replayer| {
                let process_threads: Vec<DetTid> = threads.iter().map(|(tid, _)| *tid).collect();
                replayer.next_thread_among(&process_threads, sender)
            })
            && first != thread
        {
            return ProcessSignalReceiver::ScheduleRunsFirst {
                receiver: thread,
                first,
            };
        }
        // A signal Linux discards as it is sent never reaches
        // `complete_signal` (`prepare_signal`, signal.c:1053-1054), so it
        // moves nothing and takes no thread; its copy is dropped at the send.
        let discarded = self.discarded_when_sent_to(named, signo, Some(sender));
        // Under ptrace the sender is the only thread the kernel's own
        // `complete_signal` admits, so a forwarded copy reaches it first.
        // This refusal is decided before `curr_target` moves: Linux moves
        // the pointer only on a successful walk choice, and a refused send
        // chooses nothing.
        if let ProcessSignalReceiver::Thread { thread, .. } = receiver
            && thread != sender
            && process == self.sigchld_process(sender)
            && self
                .stopped_thread_mask(sender)
                .is_none_or(|mask| mask & bit == 0)
        {
            return ProcessSignalReceiver::SenderTakesIt;
        }
        // The walk's choice becomes `curr_target` once the forwarded call has
        // succeeded (`process_signal_sent`), as Linux moves it only for a send
        // that reaches `complete_signal`.
        self.pending_signal_curr_target = moved
            .filter(|_| !discarded)
            .map(|thread| (sender, process, thread));
        if let ProcessSignalReceiver::Thread {
            thread,
            sole: false,
        } = receiver
            && !discarded
        {
            *self.process_signal_takers.entry(thread).or_default() |= bit;
        }
        tracing::info!(
            "[scheduler] process-directed signal {} from {} naming {} to process {}: {:?} (curr_target {}, threads {:?})",
            signo,
            sender,
            named,
            process,
            receiver,
            curr_target,
            threads
        );
        receiver
    }

    /// `process`'s `curr_target`: the thread the walk of `complete_signal` last
    /// chose, or the leader, where Linux starts it.
    pub(super) fn signal_curr_target(&self, process: DetPid) -> DetTid {
        self.signal_curr_targets
            .get(&process)
            .copied()
            .unwrap_or_else(|| DetTid::from_raw(process.as_raw()))
    }

    /// A send of `signo` that `sender` makes to its own process, naming
    /// `named`, and that Detcore forwards as it is because Linux gives it to
    /// the sender (`Detcore::own_process_signal`). Detcore chooses no receiver
    /// for it, but Linux's `complete_signal` still runs, and when the named
    /// task does not want the signal its walk reaches the sender, which then
    /// becomes `curr_target`, as for any walk choice once the call has
    /// succeeded (`process_signal_sent`). A later send from another process
    /// starts its walk there (https://github.com/rrnewton/hermit/issues/4034).
    /// As in `process_signal_receiver`, a standard signal the shared queue
    /// already holds (`legacy_queue`) and one Linux discards as it is sent
    /// (`prepare_signal`) reach no walk.
    pub(crate) fn own_process_signal_choice(&mut self, sender: DetTid, named: DetTid, signo: i32) {
        self.pending_signal_curr_target = None;
        if !self.models_signal_targets {
            return;
        }
        let process = self.sigchld_process(sender);
        let bit = kernel_signal_bit(signo);
        if bit & STANDARD_SIGNALS != 0
            && self
                .read_shared_pending(sender)
                .is_some_and(|pending| pending & bit != 0)
        {
            return;
        }
        if self.discarded_when_sent_to(named, signo, Some(sender)) {
            return;
        }
        let threads: Vec<(DetTid, Wants)> = self
            .process_signal_targets(process)
            .into_iter()
            .map(|thread| (thread, self.wants_signal(thread, sender, bit)))
            .collect();
        let (_, moved) = complete_signal(&threads, named, self.signal_curr_target(process));
        self.pending_signal_curr_target = moved.map(|thread| (sender, process, thread));
    }

    /// `sender`'s forwarded process-directed send to `process` succeeded: the
    /// thread the walk chose for it, if the walk chose one, becomes the
    /// process's `curr_target` (`process_signal_receiver`). A send that failed
    /// never reports, and the next choice discards its move.
    pub(crate) fn process_signal_sent(&mut self, sender: DetTid, process: DetPid) {
        if let Some((chooser, chosen_in, thread)) = self.pending_signal_curr_target.take()
            && chooser == sender
            && chosen_in == process
        {
            self.signal_curr_targets.insert(process, thread);
        }
    }

    /// The thread step 3 resumes before any other because it is to take a
    /// process-directed signal a guest sent (`process_signal_takers`,
    /// `process_signal_receiver`), if any: the first whose mark still stands.
    ///
    /// A mark stands while the copy is still in its process's shared queue
    /// and the thread still leaves the signal unblocked, so no stale mark can
    /// let a thread run first for ever: the copy goes when the thread takes
    /// it, when another thread does (which `/proc` shows), or when the signal
    /// is flushed; and a thread that blocks the signal would not take it, so
    /// another thread may, as Linux retargets a shared signal
    /// (`retarget_shared_pending`, signal.c:3090). Only a thread queued to run
    /// is chosen; one parked in a wait is woken by the ordinary path, and its
    /// mark stands until then.
    pub(super) fn first_process_signal_taker(&mut self) -> Option<DetTid> {
        let threads: Vec<DetTid> = self.process_signal_takers.keys().copied().collect();
        for thread in threads {
            let shared = self.read_shared_pending(thread).unwrap_or(0);
            let unblocked = self.stopped_thread_mask(thread).map_or(0, |mask| !mask);
            let standing = self
                .process_signal_takers
                .get(&thread)
                .copied()
                .unwrap_or(0)
                & shared
                & unblocked;
            if standing == 0 {
                self.process_signal_takers.remove(&thread);
            } else {
                self.process_signal_takers.insert(thread, standing);
            }
        }
        self.process_signal_takers
            .keys()
            .copied()
            .find(|thread| self.run_queue.contains_non_polling_tid(*thread))
    }

    /// A thread of `process` that can take a copy of the signal `bit` from
    /// the shared queue outside the turn the scheduler gives it.
    ///
    /// Its filed call installs a temporary mask that admits the signal
    /// (`Resources::blocked_signal_mask`: `rt_sigsuspend`, `ppoll`,
    /// `pselect6`, `epoll_pwait`), or publishes none the scheduler can read
    /// (`blocked_signal_mask_unknown`); or a signalfd of the process covers
    /// the signal, which any thread's read can dequeue
    /// (`signalfd_notify`, signal.c:1136); or a thread waits for it in
    /// `rt_sigtimedwait`.
    fn may_take_out_of_turn(&self, process: DetPid, bit: u64) -> Option<DetTid> {
        if self.signalfd_may_read(bit) {
            return self.process_signal_targets(process).into_iter().next();
        }
        self.process_signal_targets(process)
            .into_iter()
            .find(|thread| {
                self.sigtimedwait_sets
                    .get(thread)
                    .is_some_and(|set| set & bit != 0)
                    || self.filed_temporary_mask_admits(*thread, bit)
            })
    }

    /// Whether `thread`'s filed request carries a temporary signal mask that
    /// admits the signal `bit`, or one the scheduler cannot read.
    fn filed_temporary_mask_admits(&self, thread: DetTid, bit: u64) -> bool {
        self.next_turns
            .get(&thread)
            .and_then(|turn| turn.req.try_read())
            .and_then(Result::ok)
            .is_some_and(|request| {
                request.blocked_signal_mask_unknown
                    || request
                        .blocked_signal_mask
                        .is_some_and(|mask| mask & bit == 0)
            })
    }

    /// Whether `thread` runs outside the schedule, where the kernel can wake
    /// it for a signal at a moment host timing sets.
    fn out_of_schedule(&self, thread: DetTid) -> bool {
        self.blocked.external_io_blockers.contains_key(&thread)
            || self.blocked.rt_sigsuspend_blockers.contains_key(&thread)
    }

    /// Whether `thread` wants the signal `bit` sent now by `sender`, in the
    /// model of the module docs.
    fn wants_signal(&self, thread: DetTid, sender: DetTid, bit: u64) -> Wants {
        if self.parked.exit_fences.contains_key(&thread)
            || self.logically_killed_threads.contains(&thread)
            || self.filed_exit_request(thread)
        {
            return Wants::No;
        }
        let mask = if self.out_of_schedule(thread) {
            match self.blocked.out_of_scheduler_masks.get(&thread) {
                Some(Some(mask)) => *mask,
                _ => return Wants::Unknown,
            }
        } else {
            match self.stopped_thread_mask(thread) {
                Some(mask) => mask,
                None => return Wants::Unknown,
            }
        };
        if mask & bit != 0 {
            return Wants::No;
        }
        // Linux's `wants_signal` takes SIGKILL whatever else is pending, and
        // the running sender is `task_curr`.
        if bit == kernel_signal_bit(libc::SIGKILL) || thread == sender {
            return Wants::Yes;
        }
        if self.out_of_schedule(thread) {
            // Its own queue is not read: only the scheduler's own sends reach
            // it there, and they are recorded.
            return if self.blocked.signaled_background.contains(&thread) {
                Wants::Busy
            } else {
                Wants::Yes
            };
        }
        // A thread already chosen for a copy still in the shared queue has
        // been woken for it, so Linux passes it over
        // (`signal_wake_up`, signal.c:1027, sets `TIF_SIGPENDING`).
        if self
            .process_signal_takers
            .get(&thread)
            .is_some_and(|signals| *signals != 0)
        {
            return Wants::Busy;
        }
        match self.reliable_pending(thread) {
            Some(0) => Wants::Yes,
            Some(_) => Wants::Busy,
            None => Wants::Unknown,
        }
    }

    /// Whether `thread` `wants_signal` for a signal the kernel itself generates
    /// for its process between turns (a child's exit `SIGCHLD`, a timer's
    /// expiry; `Scheduler::kernel_signal_choice`), where `mask` is the mask
    /// Linux's `blocked` holds for it now (`None`: unreadable, so it is no
    /// target), already less any set a `rt_sigtimedwait` waits for. The rule
    /// is `wants_signal`'s, as for a guest's send, with no running sender:
    /// not exiting, not blocking the signal, and, but for SIGKILL, no other
    /// unblocked signal pending for it alone (`task_sigpending`) and not
    /// already woken for another copy in the shared queue. A thread sleeping
    /// outside the schedule is read only for its wake (`signaled_background`),
    /// since host timing can post it a signal, except a vfork parent.
    pub(super) fn kernel_sent_wants(&self, thread: DetTid, mask: Option<u64>, bit: u64) -> Wants {
        if self.parked.exit_fences.contains_key(&thread)
            || self.logically_killed_threads.contains(&thread)
            || self.filed_exit_request(thread)
        {
            return Wants::No;
        }
        let Some(mask) = mask else {
            return Wants::No;
        };
        if mask & bit != 0 {
            return Wants::No;
        }
        if bit == kernel_signal_bit(libc::SIGKILL) {
            return Wants::Yes;
        }
        // A vfork parent sleeps outside the schedule too, but no host timing
        // ends its call (the child's exec or exit, which the schedule fixes,
        // does) or posts it a signal, so its pending set is read like a
        // stopped thread's: one with a signal pending is busy, which a caught
        // signal does not wake there (`wait_for_vfork_done` sleeps killable).
        if self.out_of_schedule(thread) && !self.vfork_barriers.contains_key(&thread) {
            return if self.blocked.signaled_background.contains(&thread) {
                Wants::Busy
            } else {
                Wants::Yes
            };
        }
        if self
            .process_signal_takers
            .get(&thread)
            .is_some_and(|signals| *signals != 0)
        {
            return Wants::Busy;
        }
        match self.reliable_pending(thread) {
            Some(pending) if pending != 0 => Wants::Busy,
            _ => Wants::Yes,
        }
    }

    /// Whether `thread`'s filed request is its own exit, which Linux runs to
    /// completion: `wants_signal` passes over an exiting thread (`PF_EXITING`,
    /// signal.c:951-952).
    fn filed_exit_request(&self, thread: DetTid) -> bool {
        self.next_turns
            .get(&thread)
            .and_then(|turn| turn.req.try_read())
            .and_then(Result::ok)
            .is_some_and(|request| {
                request
                    .resources
                    .keys()
                    .any(|rid| matches!(rid, ResourceID::Exit { .. }))
            })
    }

    /// The mask of `thread`, stopped at a request or parked, as Linux's
    /// `wants_signal` reads it: the guest's own while Detcore holds the thread
    /// under a private one (`saved_guest_sigmasks`), less the set a polling
    /// `rt_sigtimedwait` waits for, which Linux unblocks for the wait
    /// (`do_sigtimedwait`, signal.c:3777).
    fn stopped_thread_mask(&self, thread: DetTid) -> Option<u64> {
        let mask = match self.saved_guest_sigmasks.get(&thread) {
            Some(saved) => *saved,
            None => self.stopped_thread_signal_state(thread).ok()?.blocked,
        };
        Some(mask & !self.sigtimedwait_sets.get(&thread).copied().unwrap_or(0))
    }

    /// The signals in `thread`'s own queue that it does not block, when it is
    /// stopped at a request or parked, without those a host-timed source may
    /// have posted (`host_timed_signals`) and Reverie's preemption marker,
    /// which the schedule does not fix, and without a copy Linux discarded as
    /// it was generated (`discarded_bits`), which the kernel or the backend
    /// still holds (a traced task's ignored signal is queued whatever its
    /// disposition) but Linux never queued, so it sets no `TIF_SIGPENDING`.
    /// A copy that was blocked when generated, and so queued although
    /// ignored, still counts (the revastra2 review of SIGSHARE S3b's fix
    /// round, delta F2).
    fn reliable_pending(&self, thread: DetTid) -> Option<u64> {
        let mask = self.stopped_thread_mask(thread)?;
        let state = self.stopped_thread_signal_state(thread).ok()?;
        let unreliable =
            self.host_timed_signals(thread) | kernel_signal_bit(reverie::PERF_EVENT_SIGNAL as i32);
        let discarded = self.discarded_bits(thread).0;
        Some(state.thread_pending & !mask & !unreliable & !discarded)
    }

    /// The kernel's signal state for `thread`, stopped at a request or
    /// parked. Unit tests script it (`test_kernel_signal_states`).
    fn stopped_thread_signal_state(&self, thread: DetTid) -> Result<KernelSignalState, Errno> {
        #[cfg(test)]
        {
            self.test_kernel_signal_states
                .get(&thread)
                .copied()
                .ok_or(Errno::ESRCH)
        }
        #[cfg(not(test))]
        {
            self.guest_signal_state(
                thread,
                self.sigchld_process(thread).as_raw(),
                thread.as_raw(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    const LEADER: i32 = 10;

    /// A receiver that is not the only thread leaving the signal unblocked.
    fn thread(tid: DetTid) -> ProcessSignalReceiver {
        ProcessSignalReceiver::Thread {
            thread: tid,
            sole: false,
        }
    }

    /// A receiver that is the only thread leaving the signal unblocked.
    fn sole(tid: DetTid) -> ProcessSignalReceiver {
        ProcessSignalReceiver::Thread {
            thread: tid,
            sole: true,
        }
    }

    fn threads(wants: &[(i32, Wants)]) -> Vec<(DetTid, Wants)> {
        wants.iter().map(|(tid, w)| (t(*tid), *w)).collect()
    }

    /// Step 1: the named task first, whatever `curr_target` says, and the
    /// walk's pointer does not move.
    #[test]
    fn the_named_thread_takes_it_when_it_wants_it() {
        let all = threads(&[(LEADER, Wants::Yes), (11, Wants::Yes), (12, Wants::Yes)]);
        assert_eq!(
            complete_signal(&all, t(LEADER), t(12)),
            (thread(t(LEADER)), None)
        );
        assert_eq!(
            complete_signal(&all, t(11), t(LEADER)),
            (thread(t(11)), None)
        );
    }

    /// Step 2: from `curr_target`, wrapping at the list's end, and the chosen
    /// thread becomes `curr_target`, so a thread that keeps wanting it keeps
    /// getting it.
    #[test]
    fn the_walk_starts_at_curr_target_and_wraps() {
        let all = threads(&[
            (LEADER, Wants::No),
            (11, Wants::Yes),
            (12, Wants::No),
            (13, Wants::Yes),
        ]);
        assert_eq!(
            complete_signal(&all, t(LEADER), t(LEADER)),
            (thread(t(11)), Some(t(11)))
        );
        assert_eq!(
            complete_signal(&all, t(LEADER), t(11)),
            (thread(t(11)), Some(t(11)))
        );
        assert_eq!(
            complete_signal(&all, t(LEADER), t(12)),
            (thread(t(13)), Some(t(13)))
        );
        let wrap = threads(&[(LEADER, Wants::No), (11, Wants::Yes), (12, Wants::No)]);
        assert_eq!(
            complete_signal(&wrap, t(LEADER), t(12)),
            (sole(t(11)), Some(t(11)))
        );
    }

    /// A `curr_target` that has exited: the walk starts at the next live
    /// thread after it, or wraps to the first.
    #[test]
    fn an_exited_curr_target_starts_the_walk_at_the_next_live_thread() {
        let all = threads(&[(LEADER, Wants::No), (11, Wants::Yes), (13, Wants::Yes)]);
        assert_eq!(
            complete_signal(&all, t(LEADER), t(12)),
            (thread(t(13)), Some(t(13)))
        );
        assert_eq!(
            complete_signal(&all, t(LEADER), t(14)),
            (thread(t(11)), Some(t(11)))
        );
    }

    /// Step 3: no thread wants it, so it stays in the shared queue.
    #[test]
    fn no_wanting_thread_leaves_it_in_the_shared_queue() {
        let all = threads(&[(LEADER, Wants::No), (11, Wants::No)]);
        assert_eq!(
            complete_signal(&all, t(LEADER), t(LEADER)),
            (ProcessSignalReceiver::SharedQueue, None)
        );
    }

    /// An unknown mask refuses only when the choice reaches it: before the
    /// thread that is chosen, or as the named task.
    #[test]
    fn an_unknown_mask_matters_only_when_the_choice_reaches_it() {
        let after = threads(&[(LEADER, Wants::No), (11, Wants::Yes), (12, Wants::Unknown)]);
        assert_eq!(
            complete_signal(&after, t(LEADER), t(LEADER)),
            (thread(t(11)), Some(t(11)))
        );
        let before = threads(&[(LEADER, Wants::No), (11, Wants::Unknown), (12, Wants::Yes)]);
        assert_eq!(
            complete_signal(&before, t(LEADER), t(LEADER)),
            (ProcessSignalReceiver::UnknownMask(t(11)), None)
        );
        let named = threads(&[(LEADER, Wants::Unknown), (11, Wants::Yes)]);
        assert_eq!(
            complete_signal(&named, t(LEADER), t(11)),
            (ProcessSignalReceiver::UnknownMask(t(LEADER)), None)
        );
    }

    /// A thread with a signal pending is passed over like one that blocks it,
    /// and when that leaves no thread the signal stays in the shared queue
    /// for the first thread that can take it.
    #[test]
    fn a_busy_thread_is_passed_over_and_leaves_the_signal_in_the_shared_queue() {
        let all = threads(&[(LEADER, Wants::Busy), (11, Wants::No), (12, Wants::Yes)]);
        assert_eq!(
            complete_signal(&all, t(LEADER), t(LEADER)),
            (thread(t(12)), Some(t(12)))
        );
        let none = threads(&[(LEADER, Wants::Busy), (11, Wants::No)]);
        assert_eq!(
            complete_signal(&none, t(LEADER), t(LEADER)),
            (ProcessSignalReceiver::SharedQueue, None)
        );
    }

    /// A receiver is the sole one only when every other thread blocks the
    /// signal or is exiting: a busy thread, or one whose mask is unknown, could
    /// still take a shared copy.
    #[test]
    fn the_receiver_is_sole_only_when_every_other_thread_blocks_it() {
        let blocked = threads(&[(LEADER, Wants::No), (11, Wants::Yes), (12, Wants::No)]);
        assert_eq!(
            complete_signal(&blocked, t(11), t(LEADER)),
            (sole(t(11)), None)
        );
        for other in [Wants::Yes, Wants::Busy, Wants::Unknown] {
            let all = threads(&[(LEADER, Wants::No), (11, Wants::Yes), (12, other)]);
            assert_eq!(
                complete_signal(&all, t(11), t(LEADER)),
                (thread(t(11)), None),
                "{other:?}"
            );
        }
    }

    /// A named task that is not live (an exited leader) is passed over.
    #[test]
    fn a_named_task_that_is_gone_is_passed_over() {
        let all = threads(&[(11, Wants::No), (12, Wants::Yes)]);
        assert_eq!(
            complete_signal(&all, t(LEADER), t(LEADER)),
            (sole(t(12)), Some(t(12)))
        );
    }
}
