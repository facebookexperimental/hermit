/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The admission of a guest's signal to another process where the backend
//! keeps the guest's signal queues itself (KVM): sections 2 and 5 of the KVM
//! cross-process signal design
//! (`ai_docs/plans/2026-11-10-kvm-cross-process-signal-delivery.md` in the
//! dev-hermit workspace), stage X1 slice C.
//!
//! Such a backend performs a guest's `kill`, `tkill`, `tgkill`,
//! `rt_sigqueueinfo`, `rt_tgsigqueueinfo` or `pidfd_send_signal` that names
//! another process, or names its own process while that has another thread,
//! only once the Tool declares that it selects the receiving thread
//! (`ProcessSignalControl::enable_cross_process_signals`, which
//! `Scheduler::install_signal_control` asks for). It then queues the signal
//! as Linux's `__send_signal_locked` does, on the process's shared queue or
//! the named thread's own, and wakes no thread: a thread takes it only at a
//! boundary the scheduler authorizes (`authorize_signal_boundary`), or in a
//! parked observation. So in the sender's turn, before the call is
//! forwarded, the scheduler decides whether the delivery Linux would make is
//! one it can order now (`Scheduler::guest_send_admission`):
//!
//! 1. What Linux does with the signal as it sends it
//!    (`kernel/signal.c`, `prepare_signal` and `__send_signal_locked`): a
//!    signal the named task ignores and does not block is discarded; a
//!    standard signal its queue already holds is not queued again; one that
//!    every thread that could take it blocks stays queued. None of these
//!    needs a thread to act now.
//! 2. The transition its disposition requires: a handler (*catch*); a
//!    default stop; a default action that ends the process (*die*); or
//!    SIGKILL (*kill*). Only *catch* is admitted at this stage: *stop* waits
//!    for stage X2 (the backend refuses it too), *die* for slice F and *kill*
//!    for slice G. A receiver waiting for the signal in `rt_sigtimedwait`,
//!    which its own mask blocks outside the wait, takes even a default-fatal
//!    signal by that wait, as Linux's `complete_signal` leaves the group
//!    alone for a thread whose `real_blocked` holds the signal; that is a
//!    catch too.
//! 3. The receiver: the named thread for a thread-directed send, and for a
//!    process-directed one the thread Linux's `complete_signal` chooses
//!    (`process_signal_receiver::complete_signal`), each thread's wish read
//!    under the mask Linux's `blocked` holds for it now, a call's temporary
//!    mask included (`Scheduler::controlled_wants`). Where it chooses no
//!    thread, the copy waits in the shared queue for the first thread that
//!    can take it, as in Linux. A send to the sender's own process that the
//!    sender does not block, but that Linux gives to another thread, is
//!    refused (`SendRefusal::SenderTakesIt`): the sender's own delivery
//!    permit would take the copy at its return first.
//! 4. Whether the receiver's state lets it take the copy without a later
//!    slice (`CatchReach`): at a boundary of its own (runnable, a posted
//!    continuation, a delivery permit's holder, or held at a happens-before
//!    gate), at its next probe (`pselect6`, a masked `ppoll`,
//!    `rt_sigtimedwait`), through the drain (an internal IO poll with the
//!    restart marker, a backend-served `rt_sigsuspend`). Every other state
//!    is refused by name (`SendRefusal`), a raw `select` included
//!    (`SendRefusal::RawSelect`).
//!
//! A signalfd reads its reader's own queue and its process's shared one. A
//! read of a blocking signalfd runs outside the schedule, as a host call, and
//! so does a host wait that includes a signalfd, which the backend wakes for
//! a covered signal: either can take the copy, or see it, at a moment host
//! timing sets. So where a signalfd may cover the signal, a send is refused
//! while a thread that can read it is in a host call
//! (`SendRefusal::SignalfdReadOutsideTurn`), and a caught copy is refused
//! outright (`SendRefusal::SignalfdCovered`): such a read could begin before
//! the receiver takes the copy.
//!
//! A refused send has no effect: nothing is queued, `curr_target` does not
//! move, and no thread is marked.
//!
//! An admitted copy that its receiver is to take is claimed for it once the
//! forwarded call succeeds (`Scheduler::guest_send_committed`), until the
//! receiver takes it. A receiver chosen while runnable can still enter a wait
//! before its next boundary: this backend's emulated `pause` and `nanosleep`
//! do not look for a pending signal before they park
//! (`supports_blocked_wait_signal_interruption`), and the observation that
//! ends such a wait for a signal of the thread's own looks at its own queue
//! only. Linux returns from such a wait at once. So at every pass that can
//! begin a parked observation the claims are checked again
//! (`Scheduler::audit_guest_send_claims`), and a claimed copy whose receiver
//! now waits where it cannot take it ends the run with that refusal, as the
//! design's retargeting rule ends it for a receiver in a refused state.
//! Linux would also wake another thread for a shared copy whose receiver
//! exited or blocked the signal first (`retarget_shared_pending`), which is
//! slice E; until then that case is refused too.
//!
//! An observation begun for another reason (a SIGALRM, a parent-death signal)
//! takes the lowest signal its thread leaves unblocked, from its own queue
//! and then the shared one, which can be a copy claimed for another thread.
//! Which of the two claims Linux settles first is the design's opportunity
//! rule (slice D2); until then such an observation is refused
//! (`Scheduler::contested_guest_send_claim`).
//!
//! A claim is a wake choice, not a reservation: it names the thread the
//! scheduler wakes for the copy and does not reserve the copy for it.
//! Linux's `complete_signal` only chooses which thread to wake and wakes it
//! (`signal_wake_up`, kernel/signal.c:963-1032 at v7.1.3); the copy stays in
//! the process's shared queue. Only the chosen thread is woken and flagged:
//! `signal_wake_up_state` sets its `TIF_SIGPENDING` (:725), the per-task flag
//! `signal_pending` reads (include/linux/sched/signal.h:382-396) and a return
//! to user mode acts on (kernel/entry/common.c:63-64). An unchosen thread
//! asleep in a call, a probe's sleep included, is not woken and does not
//! take the copy. Another thread takes it only on a return to user mode with
//! its own flag set, as after a change of its mask that leaves the signal
//! unblocked (`__set_task_blocked` recalculates the flag, :3188-3197,
//! through `recalc_sigpending_tsk`, :159-166); it then dequeues the copy
//! (`dequeue_signal` reads the thread's own queue and then
//! `signal->shared_pending`, :618-640). So an ordinary boundary of another
//! thread of the process that leaves the signal unblocked, which the
//! scheduler authorizes without reading the claims
//! (`authorize_signal_boundary`), can take a copy claimed for a receiver
//! that would take it at its next probe or once a gate releases it, if
//! step 3 runs that thread first; the next audit finds the copy gone and
//! drops the claim, and the run goes on. Linux permits that outcome for a
//! thread whose own flag is set, as above. For a running thread whose flag
//! nothing has recalculated, Linux would leave the copy to the chosen
//! thread; deciding that case is follow-up work (the contested-claim rule
//! at ordinary boundaries). That boundary is in its own thread's serial
//! grant, a point the schedule fixes. Only an observation, whose dequeue the
//! observed thread's host thread makes, is refused (above).
//!
//! A probe stands for a sleeping call Linux will not interrupt, enforced by
//! ORDER. Linux leaves a sibling asleep in `select` or `pselect6`: its
//! `do_select` ends the sleep only on its own `signal_pending`
//! (fs/select.c:575, :603). Here the sibling's probe is a real call that
//! takes any pending copy its mask lets in, so the chosen thread runs first
//! instead (`Scheduler::first_process_signal_taker`, which on a controlled
//! backend returns a polling taker too) and its call takes the copy before
//! the sibling probes. Order is all that enforces it: a thread step 3 ranks
//! above the process taker (a group-exit decider, a reserved SIGALRM probe,
//! a SIGCHLD taker, a taker with a lower `DetTid`) still runs first and can
//! take the copy, as a thread Linux woke for a reason of its own can.

use std::fmt;

use reverie::syscalls::Errno;
use serde::Deserialize;
use serde::Serialize;

use super::Scheduler;
use super::discarded_at_send::STANDARD_SIGNALS;
use super::kernel_signal_bit;
use super::parked::ControlCapability;
use super::parked::NextTurnOwner;
use super::process_signal_receiver::ProcessSignalReceiver;
use super::process_signal_receiver::Wants;
use super::process_signal_receiver::complete_signal;
use crate::resources::ResourceID;
use crate::types::DetPid;
use crate::types::DetTid;

/// Whether a guest's send names one thread or its process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendScope {
    /// `tkill`, `tgkill`, `rt_tgsigqueueinfo`, or `pidfd_send_signal` with the
    /// thread scope: the copy joins the named thread's own queue.
    Thread,
    /// `kill`, `rt_sigqueueinfo`, or `pidfd_send_signal` with the process
    /// scope: the copy joins the process's shared queue.
    Process,
}

/// How an admitted receiver takes a caught copy (the rows of section 2's
/// table that slice C admits).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatchReach {
    /// Runnable, a continuation posted and not yet granted, or a delivery
    /// permit's holder waiting in its signal hook: at a later boundary of its
    /// own.
    Boundary,
    /// Held at a happens-before gate: at its boundary once it is released.
    Gate,
    /// Probing in `pselect6`, a masked `ppoll` or
    /// `rt_sigtimedwait`: its next probe takes the copy.
    Probe,
    /// Parked in an internal IO poll with the restart marker: the drain
    /// answers its turn `Signaled` (`Scheduler::notify_signal_pending`).
    RestartablePoll,
    /// In a backend-served `rt_sigsuspend`: the drain wakes it
    /// (`Scheduler::wake_backend_signal_waits`).
    BackendSuspend,
}

/// Why a guest's send is refused at admission, each naming the state and
/// what lifts the refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendRefusal {
    /// An exec is in flight in the target process; its `de_thread` decides
    /// which threads remain.
    ExecInFlight,
    /// SIGKILL to another process: the kill transition, slice G.
    Kill,
    /// A default action that ends the process: the die transition, slice F.
    Die,
    /// A default stop: stage X2.
    Stop,
    /// This thread's signal mask is unknown.
    UnknownMask(DetTid),
    /// This thread's signal state, or what it waits in, cannot be read.
    UnknownState(DetTid),
    /// The sender does not block the signal, and Linux gives it to this other
    /// thread of the sender's own process.
    SenderTakesIt(DetTid),
    /// The receiver is parked in an emulated `pause`, `nanosleep`, polled read
    /// or child wait: slice D.
    Parked(DetTid),
    /// The receiver polls in a call whose probe never takes a pending signal
    /// (`poll`, `epoll_wait`, an unmasked `ppoll`): slice D.
    Poll(DetTid),
    /// The receiver waits in a raw `select`, which has no signal mask the
    /// backend's probe can check, so no probe of it takes the copy: refused
    /// until its probes are lowered to `pselect6`
    /// (<https://github.com/rrnewton/hermit/issues/5452>).
    RawSelect(DetTid),
    /// The receiver is a vfork parent waiting for its child, which no slice
    /// of stage X1 admits as a receiver.
    VforkParent(DetTid),
    /// The receiver waits on a futex (N8a).
    Futex(DetTid),
    /// The receiver is in a host call outside the schedule.
    ExternalIo(DetTid),
    /// The receiver polls outside the run queue.
    PollerOutsideRunQueue(DetTid),
    /// The receiver is entering a background call.
    BackgroundEntry(DetTid),
    /// A signalfd may cover the signal, and this thread, which can read it
    /// (its own queue, and its process's), is in a host call outside the
    /// schedule.
    SignalfdReadOutsideTurn(DetTid),
    /// A signalfd may cover the caught signal that this thread is chosen to
    /// take.
    SignalfdCovered(DetTid),
}

impl fmt::Display for SendRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendRefusal::ExecInFlight => write!(f, "an exec is in flight in the process"),
            SendRefusal::Kill => write!(
                f,
                "KVM cannot yet end another process for a SIGKILL (stage X1 slice G)"
            ),
            SendRefusal::Die => write!(
                f,
                "its default action ends the process, which KVM cannot yet carry out for a \
                 signal another process sends (stage X1 slice F)"
            ),
            SendRefusal::Stop => write!(
                f,
                "its default action stops the process, which KVM does not support (stage X2)"
            ),
            SendRefusal::UnknownMask(thread) => {
                write!(f, "thread {thread}'s signal mask is unknown")
            }
            SendRefusal::UnknownState(thread) => write!(
                f,
                "Detcore cannot read thread {thread}'s signal state or tell what it waits in"
            ),
            SendRefusal::SenderTakesIt(thread) => write!(
                f,
                "the sending thread does not block it, so its own delivery permit takes the \
                 copy at its return, before thread {thread}, the thread Linux's complete_signal \
                 chooses"
            ),
            SendRefusal::Parked(thread) => write!(
                f,
                "thread {thread}, which takes it, is parked in an emulated pause, nanosleep, \
                 polled read or child wait, which KVM does not yet end for a signal another \
                 process sends (stage X1 slice D)"
            ),
            SendRefusal::Poll(thread) => write!(
                f,
                "thread {thread}, which takes it, polls in a call whose probe never takes a \
                 pending signal (poll, epoll_wait, an unmasked ppoll), which KVM does not yet \
                 end for a signal another process sends (stage X1 slice D)"
            ),
            SendRefusal::RawSelect(thread) => write!(
                f,
                "thread {thread}, which takes it, waits in select, and select has no signal \
                 mask the backend can check; use pselect6 \
                 (https://github.com/rrnewton/hermit/issues/5452)"
            ),
            SendRefusal::VforkParent(thread) => write!(
                f,
                "thread {thread}, which takes it, is a vfork parent waiting for its child, which \
                 KVM does not yet admit as the receiver of a signal another process sends"
            ),
            SendRefusal::Futex(thread) => write!(
                f,
                "thread {thread}, which takes it, waits on a futex, and KVM cannot interrupt a \
                 futex wait for a signal (N8a)"
            ),
            SendRefusal::ExternalIo(thread) => write!(
                f,
                "thread {thread}, which takes it, is in a host call outside the schedule, which \
                 Detcore cannot end"
            ),
            SendRefusal::PollerOutsideRunQueue(thread) => write!(
                f,
                "thread {thread}, which takes it, polls outside the run queue"
            ),
            SendRefusal::BackgroundEntry(thread) => write!(
                f,
                "thread {thread}, which takes it, is entering a background call"
            ),
            SendRefusal::SignalfdReadOutsideTurn(thread) => write!(
                f,
                "a signalfd may cover it, and thread {thread}, which can read it, is in a host \
                 call outside the schedule, where a signalfd read or wait could take the copy at a \
                 moment host timing sets"
            ),
            SendRefusal::SignalfdCovered(thread) => write!(
                f,
                "a signalfd may cover it, and a read of that signalfd outside the schedule could \
                 take the copy before thread {thread}, which Linux's complete_signal chooses, \
                 at a moment host timing sets"
            ),
        }
    }
}

/// The scheduler's decision on a guest's send (`Scheduler::guest_send_admission`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendAdmission {
    /// Not a send this admission decides: the backend keeps no signal queues
    /// of its own, or performs the send within the sender's process as it
    /// always has (a thread-directed send there, or a process-directed one
    /// while the sender is its only thread), or the target has no live
    /// thread, which Detcore answers from its model before this.
    NotDecided,
    /// Admitted, and no thread has to act now: the signal is discarded as it
    /// is sent, coalesced with a copy already queued, blocked by every thread
    /// that could take it, ignored when it is delivered, or left in the
    /// shared queue for the first thread that can take it.
    NoTransition,
    /// Admitted: `receiver` takes the caught copy, as `reach` says.
    Catch { receiver: DetTid, reach: CatchReach },
    /// Refused by name, before any effect.
    Refused(SendRefusal),
}

/// The copies a guest sent from another process that wait for the thread the
/// admission chose to take them (`Scheduler::guest_send_committed`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuestSendClaim {
    /// Signals in the thread's process's shared queue.
    pub(crate) shared: u64,
    /// Signals in the thread's own queue.
    pub(crate) private: u64,
}

/// Why a claimed copy can no longer be delivered as its admission decided
/// (`Scheduler::audit_guest_send_claims`,
/// `Scheduler::contested_guest_send_claim`). Ends the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClaimRefusal {
    /// The thread the copy was chosen for.
    pub(crate) receiver: DetTid,
    /// The claimed signal.
    pub(crate) signal: i32,
    pub(crate) reason: ClaimReason,
}

/// What [`ClaimRefusal`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimReason {
    /// The receiver now waits in a state the admission refuses.
    Waits(SendRefusal),
    /// The receiver exited, or blocks the signal, before taking a shared
    /// copy that another live thread leaves unblocked: Linux wakes that
    /// thread for it (`retarget_shared_pending`), slice E.
    Retarget,
    /// An observation of this other thread, begun for a signal of its own
    /// or of the scheduler's, would take the claimed copy first: slice D2.
    Contested { observed: DetTid },
}

impl fmt::Display for ClaimRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HERMIT_KVM_SIGNAL_CLAIM_REFUSED: signal {} that another process sent is not \
             supported: thread {} was chosen to take it, but ",
            self.signal, self.receiver
        )?;
        match self.reason {
            ClaimReason::Waits(refusal) => write!(
                f,
                "it began to wait before taking it, where it cannot take it: {refusal}"
            ),
            ClaimReason::Retarget => write!(
                f,
                "it exited or blocked the signal before taking it, and Linux wakes another \
                 thread for the copy (retarget_shared_pending), which KVM does not yet do \
                 (stage X1 slice E)"
            ),
            ClaimReason::Contested { observed } => write!(
                f,
                "a parked observation of thread {observed} would take it first, and which of \
                 the two claims Linux settles first is not yet decided (stage X1 slice D2)"
            ),
        }
    }
}

/// The lowest signal in `bits`, in Linux's numbering.
fn lowest_signal(bits: u64) -> i32 {
    bits.trailing_zeros() as i32 + 1
}

/// Whether `signal`'s default action stops the process.
fn default_stop(signal: i32) -> bool {
    matches!(
        signal,
        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU
    )
}

impl Scheduler {
    /// Section 2's admission of the guest send of `signo` that `sender` makes
    /// naming `named` (module docs), decided in `sender`'s turn and with no
    /// effect. `process` is the process the call names, or `None` to take the
    /// named thread's.
    pub(crate) fn guest_send_admission(
        &self,
        sender: DetTid,
        process: Option<DetPid>,
        named: DetTid,
        scope: SendScope,
        signo: i32,
    ) -> SendAdmission {
        if !self.signal_control_installed() || !(1..=64).contains(&signo) {
            return SendAdmission::NotDecided;
        }
        let Some(process) = process.or_else(|| self.registered_process(named)) else {
            return SendAdmission::NotDecided;
        };
        let threads = self.process_signal_targets(process);
        let own = process == self.sigchld_process(sender);
        let decided =
            !own || (scope == SendScope::Process && threads.iter().any(|thread| *thread != sender));
        if !decided || threads.is_empty() {
            return SendAdmission::NotDecided;
        }
        if scope == SendScope::Thread && !threads.contains(&named) {
            return SendAdmission::NotDecided;
        }
        if self.exec_teardowns.contains_key(&process) {
            return SendAdmission::Refused(SendRefusal::ExecInFlight);
        }
        if signo == libc::SIGKILL {
            return SendAdmission::Refused(SendRefusal::Kill);
        }
        // Discarded as it is sent (`sig_ignored`, judged on the named task).
        if self.discarded_when_sent_to(named, signo, Some(sender)) {
            return SendAdmission::NoTransition;
        }
        let bit = kernel_signal_bit(signo);
        // Dispositions are the process's, and the named task's queue is the
        // one a thread-directed copy joins.
        let reader = if threads.contains(&named) {
            named
        } else {
            threads[0]
        };
        let Ok(state) = self.read_guest_signal_state(reader) else {
            return SendAdmission::Refused(SendRefusal::UnknownState(reader));
        };
        // A standard signal already in the queue the copy would join is not
        // queued again (`legacy_queue`).
        let queue = match scope {
            SendScope::Thread => state.thread_pending,
            SendScope::Process => state.shared_pending,
        };
        if bit & STANDARD_SIGNALS != 0 && queue & bit != 0 {
            return SendAdmission::NoTransition;
        }
        // A signalfd read of a blocking descriptor runs outside the schedule
        // (`IOAction::Blocking`), and so does a host wait that includes a
        // signalfd, which the backend wakes for a covered signal: either can
        // take the copy, or see it, at a moment host timing sets. A signalfd
        // reads its reader's own queue and its process's shared one
        // (`signalfd_dequeue`, fs/signalfd.c).
        let signalfd = self.signalfd_may_read(bit);
        if signalfd {
            let in_host_call = |thread: &DetTid| {
                self.blocked.external_io_blockers.contains_key(thread)
                    && !self.vfork_barriers.contains_key(thread)
            };
            let reader = match scope {
                SendScope::Thread => Some(named).filter(in_host_call),
                SendScope::Process => threads.iter().copied().find(in_host_call),
            };
            if let Some(reader) = reader {
                return SendAdmission::Refused(SendRefusal::SignalfdReadOutsideTurn(reader));
            }
        }
        let wants: Vec<(DetTid, Wants)> = match scope {
            SendScope::Thread => vec![(named, self.controlled_wants(named, sender, bit))],
            SendScope::Process => threads
                .iter()
                .map(|thread| (*thread, self.controlled_wants(*thread, sender, bit)))
                .collect(),
        };
        // Blocked by every thread that could take it, or every such thread is
        // exiting: it stays queued and no thread is woken.
        if wants.iter().all(|(_, wants)| *wants == Wants::No) {
            return SendAdmission::NoTransition;
        }
        let caught = state.caught & bit != 0;
        if !caught {
            // Ignored when it is delivered: nothing to carry out.
            if state.ignored_at_generation() & bit != 0 {
                return SendAdmission::NoTransition;
            }
            if default_stop(signo) {
                return SendAdmission::Refused(SendRefusal::Stop);
            }
        }
        let receiver = match scope {
            SendScope::Thread => match wants[0].1 {
                Wants::Unknown => return SendAdmission::Refused(SendRefusal::UnknownMask(named)),
                Wants::Yes | Wants::Busy | Wants::No => Some(named),
            },
            SendScope::Process => {
                match complete_signal(&wants, named, self.signal_curr_target(process)).0 {
                    ProcessSignalReceiver::Thread { thread, .. } => Some(thread),
                    ProcessSignalReceiver::UnknownMask(thread) => {
                        return SendAdmission::Refused(SendRefusal::UnknownMask(thread));
                    }
                    _ => None,
                }
            }
        };
        // A default action that ends the process ends it whichever thread
        // takes the copy, now or later, but for a thread that waits for the
        // signal in `rt_sigtimedwait` and blocks it outside the wait: Linux's
        // `complete_signal` ends the group only through a thread whose
        // `real_blocked` leaves the signal out, and the wait dequeues it
        // (`do_sigtimedwait`).
        if !caught && !receiver.is_some_and(|receiver| self.waits_for_blocked_signal(receiver, bit))
        {
            return SendAdmission::Refused(SendRefusal::Die);
        }
        // No thread wants it now: the first that can takes it from the
        // shared queue, as in Linux.
        let Some(receiver) = receiver else {
            return SendAdmission::NoTransition;
        };
        if signalfd {
            return SendAdmission::Refused(SendRefusal::SignalfdCovered(receiver));
        }
        if receiver == sender {
            return SendAdmission::Catch {
                receiver,
                reach: CatchReach::Boundary,
            };
        }
        if own
            && self
                .controlled_mask(sender)
                .is_none_or(|mask| mask & bit == 0)
        {
            return SendAdmission::Refused(SendRefusal::SenderTakesIt(receiver));
        }
        match self.catch_reach(receiver) {
            Ok(reach) => SendAdmission::Catch { receiver, reach },
            Err(refusal) => SendAdmission::Refused(refusal),
        }
    }

    /// The mask Linux's `blocked` holds for `thread` now, where a process
    /// signal control is installed: a call's temporary mask where one is
    /// installed, and otherwise the thread's own
    /// (`read_guest_signal_mask`), less the set a polling `rt_sigtimedwait`
    /// waits for, which Linux unblocks for the wait (`do_sigtimedwait`).
    /// `None` where it cannot be read.
    pub(super) fn controlled_mask(&self, thread: DetTid) -> Option<u64> {
        let mask = self.read_guest_signal_mask(thread)?;
        Some(mask & !self.sigtimedwait_sets.get(&thread).copied().unwrap_or(0))
    }

    /// Whether `thread` waits for the signal `bit` in `rt_sigtimedwait` while
    /// its own mask, Linux's `real_blocked` for the wait, blocks it.
    fn waits_for_blocked_signal(&self, thread: DetTid, bit: u64) -> bool {
        self.sigtimedwait_sets
            .get(&thread)
            .is_some_and(|set| set & bit != 0)
            && self
                .read_guest_signal_mask(thread)
                .is_some_and(|mask| mask & bit != 0)
    }

    /// Whether `thread` wants the signal `bit` that `sender` sends now
    /// (`wants_signal`, kernel/signal.c), where a process signal control is
    /// installed: not exiting, not blocking it under the mask Linux's
    /// `blocked` holds now (`controlled_mask`), and, but for the running
    /// sender (`task_curr`), no signal of its own to take first and no copy
    /// it was already chosen for (`task_sigpending`). A thread running a call
    /// outside the schedule is never busy: the backend can post it a signal
    /// when host timing ends that call, so its queue is not the schedule's.
    pub(super) fn controlled_wants(&self, thread: DetTid, sender: DetTid, bit: u64) -> Wants {
        if self.parked.exit_fences.contains_key(&thread)
            || self.logically_killed_threads.contains(&thread)
            || self.filed_exit_request(thread)
        {
            return Wants::No;
        }
        let Some(mask) = self.controlled_mask(thread) else {
            return Wants::Unknown;
        };
        if mask & bit != 0 {
            return Wants::No;
        }
        if thread == sender {
            return Wants::Yes;
        }
        if (self.blocked.external_io_blockers.contains_key(&thread)
            || self.blocked.rt_sigsuspend_blockers.contains_key(&thread))
            && !self.vfork_barriers.contains_key(&thread)
        {
            return Wants::Yes;
        }
        let chosen = self
            .process_signal_takers
            .get(&thread)
            .is_some_and(|signals| *signals != 0)
            || self
                .guest_send_claims
                .get(&thread)
                .is_some_and(|claim| claim.shared | claim.private != 0);
        if chosen {
            return Wants::Busy;
        }
        match self.read_guest_signal_state(thread) {
            Ok(state) if self.own_signals_to_take(thread, &state, mask) != 0 => Wants::Busy,
            Ok(_) => Wants::Yes,
            Err(_) => Wants::Unknown,
        }
    }

    /// How `thread` can take a caught copy, by its state (the rows of section
    /// 2's table), from the scheduler's committed state: the pools of calls
    /// outside the schedule, the parked request's origin, the futex waiters,
    /// a backend-served `rt_sigsuspend`, the happens-before gates and
    /// run-queue membership. A queued thread's request is read only for which
    /// call it polls: it filed that request in its own turn. A state no row
    /// admits is refused, an unknown one included.
    pub(super) fn catch_reach(&self, thread: DetTid) -> Result<CatchReach, SendRefusal> {
        if self.vfork_barriers.contains_key(&thread) {
            return Err(SendRefusal::VforkParent(thread));
        }
        if self.background_entry_barriers.contains_key(&thread) {
            return Err(SendRefusal::BackgroundEntry(thread));
        }
        if self.blocked.external_io_blockers.contains_key(&thread)
            || self.blocked.rt_sigsuspend_blockers.contains_key(&thread)
        {
            return Err(SendRefusal::ExternalIo(thread));
        }
        let Some(turn) = self.next_turns.get(&thread) else {
            return Err(SendRefusal::UnknownState(thread));
        };
        if turn.protocol.owner == NextTurnOwner::Ordinary
            && turn.protocol.origin.is_some_and(|origin| {
                matches!(
                    origin.control,
                    ControlCapability::ParkedWait { .. }
                        | ControlCapability::PolledRead { .. }
                        | ControlCapability::ChildWait { .. }
                )
            })
            && !turn.req.is_empty()
            && !turn.resp.is_full()
        {
            return Err(SendRefusal::Parked(thread));
        }
        if self
            .parked_futex_waiter(thread)
            .is_some_and(|waiter| !waiter.woken)
        {
            return Err(SendRefusal::Futex(thread));
        }
        if self.backend_suspend_mask(thread).is_some() {
            return Ok(CatchReach::BackendSuspend);
        }
        if self
            .happens_before
            .as_ref()
            .is_some_and(|hb| hb.parked.contains(&thread))
        {
            return Ok(CatchReach::Gate);
        }
        let request = turn.req.try_read().and_then(Result::ok);
        let lone_poll = request.as_ref().is_some_and(|request| {
            request.resources.len() == 1
                && request
                    .resources
                    .contains_key(&ResourceID::InternalIOPolling)
        });
        if !self.run_queue.contains_tid(thread) {
            return Err(if lone_poll {
                SendRefusal::PollerOutsideRunQueue(thread)
            } else {
                SendRefusal::UnknownState(thread)
            });
        }
        let Some(request) = request else {
            return Err(SendRefusal::UnknownState(thread));
        };
        if !lone_poll {
            return Ok(CatchReach::Boundary);
        }
        // A raw `select` has no mask of its own, and the backend's probe of
        // it never takes a pending signal: no probe ends its wait.
        if request.fyi.as_str() == "select" {
            return Err(SendRefusal::RawSelect(thread));
        }
        // Each probe of these calls applies the call's mask and takes a
        // pending signal it lets in (a `ppoll` only under a mask of its own:
        // `Detcore::backend_masked_ppoll_wait`).
        let probes = request.fyi.as_str() == "rt_sigtimedwait"
            || (request.signal_interrupt_errno() == Some(Errno::EINTR.into_raw())
                && (request.fyi.as_str() == "pselect6"
                    || (request.fyi.as_str() == "ppoll" && request.blocked_signal_mask.is_some())));
        if probes {
            return Ok(CatchReach::Probe);
        }
        if self.restartable_internal_io_signals(thread).is_some() {
            return Ok(CatchReach::RestartablePoll);
        }
        Err(SendRefusal::Poll(thread))
    }

    /// The guest send that `guest_send_admission` admitted for `receiver` to
    /// take succeeded: the copy is claimed for it until it takes it
    /// (`audit_guest_send_claims`), and Linux's `signal_wake_up` makes it busy
    /// for a later choice (`controlled_wants`).
    pub(crate) fn guest_send_committed(&mut self, receiver: DetTid, scope: SendScope, signo: i32) {
        if !self.signal_control_installed() || !(1..=64).contains(&signo) {
            return;
        }
        let bit = kernel_signal_bit(signo);
        let claim = self.guest_send_claims.entry(receiver).or_default();
        match scope {
            SendScope::Thread => claim.private |= bit,
            SendScope::Process => claim.shared |= bit,
        }
    }

    /// Check every claimed copy again (module docs), at a pass that may begin
    /// a parked observation, where no guest thread holds the turn: drop what
    /// was taken, flushed, or is blocked now, and find a receiver that now
    /// waits where it cannot take its copy, or a shared copy left behind by a
    /// receiver that exited or blocked it while another live thread leaves
    /// it unblocked. A thread parked where the observation of its own signals
    /// reaches it (`observe_parked_thread_woken_by_its_own_signal`) takes a
    /// copy in its own queue there, so only a shared copy is refused for it.
    pub(crate) fn audit_guest_send_claims(&mut self) -> Option<ClaimRefusal> {
        let claims: Vec<(DetTid, GuestSendClaim)> = self
            .guest_send_claims
            .iter()
            .map(|(thread, claim)| (*thread, *claim))
            .collect();
        for (thread, claim) in claims {
            let process = self.sigchld_process(thread);
            let live = self.process_signal_targets(process).contains(&thread)
                && !self.parked.exit_fences.contains_key(&thread)
                && !self.logically_killed_threads.contains(&thread)
                && !self.filed_exit_request(thread);
            let (standing, refusal) = if live {
                self.standing_claim(thread, process, claim)
            } else {
                // Its own queue went with it; a shared copy stays.
                let shared = self
                    .read_shared_pending_of(process, thread)
                    .map_or(0, |pending| claim.shared & pending);
                let refusal = (shared != 0)
                    .then(|| self.retarget_refusal(thread, process, shared))
                    .flatten();
                (GuestSendClaim::default(), refusal)
            };
            if standing == GuestSendClaim::default() {
                self.guest_send_claims.remove(&thread);
            } else {
                self.guest_send_claims.insert(thread, standing);
            }
            if refusal.is_some() {
                return refusal;
            }
        }
        None
    }

    /// What is left of `claim` for `thread`, a live thread of `process`, and
    /// the refusal it now meets, if any (`audit_guest_send_claims`).
    fn standing_claim(
        &self,
        thread: DetTid,
        process: DetPid,
        claim: GuestSendClaim,
    ) -> (GuestSendClaim, Option<ClaimRefusal>) {
        let (Ok(state), Some(mask)) = (
            self.read_guest_signal_state(thread),
            self.controlled_mask(thread),
        ) else {
            // Not readable now: judged at the next pass.
            return (claim, None);
        };
        let shared = claim.shared & state.shared_pending;
        let private = claim.private & state.thread_pending;
        // Blocked now: a copy in its own queue waits for it, and a shared one
        // for whichever thread unblocks it first.
        if shared & mask != 0
            && let Some(refusal) = self.retarget_refusal(thread, process, shared & mask)
        {
            return (GuestSendClaim::default(), Some(refusal));
        }
        let standing = GuestSendClaim {
            shared: shared & !mask,
            private: private & !mask,
        };
        if standing == GuestSendClaim::default() {
            return (standing, None);
        }
        let refusal = match self.catch_reach(thread) {
            Ok(_) | Err(SendRefusal::UnknownState(_)) => None,
            Err(SendRefusal::Parked(_)) if standing.shared == 0 => None,
            Err(SendRefusal::Parked(_)) => Some((standing.shared, SendRefusal::Parked(thread))),
            Err(refusal) => Some((standing.shared | standing.private, refusal)),
        };
        (
            standing,
            refusal.map(|(bits, refusal)| ClaimRefusal {
                receiver: thread,
                signal: lowest_signal(bits),
                reason: ClaimReason::Waits(refusal),
            }),
        )
    }

    /// The refusal for shared copies `bits` that `thread` was chosen for and
    /// can no longer take, where another live thread of `process` leaves one
    /// of them unblocked: Linux wakes that thread (`retarget_shared_pending`).
    fn retarget_refusal(&self, thread: DetTid, process: DetPid, bits: u64) -> Option<ClaimRefusal> {
        let unblocked = self
            .process_signal_targets(process)
            .into_iter()
            .filter(|other| *other != thread)
            .filter_map(|other| self.controlled_mask(other))
            .fold(0, |unblocked, mask| unblocked | (bits & !mask));
        (unblocked != 0).then(|| ClaimRefusal {
            receiver: thread,
            signal: lowest_signal(unblocked),
            reason: ClaimReason::Retarget,
        })
    }

    /// `process`'s shared queue, read through `thread` if it still has a
    /// signal state and otherwise through the process's first live thread.
    fn read_shared_pending_of(&self, process: DetPid, thread: DetTid) -> Option<u64> {
        self.read_guest_signal_state(thread)
            .ok()
            .or_else(|| {
                let first = self.process_signal_targets(process).into_iter().next()?;
                self.read_guest_signal_state(first).ok()
            })
            .map(|state| state.shared_pending)
    }

    /// A shared copy claimed for a thread of `pid` other than `observed` that
    /// an observation of `observed`, or a wake of it for a signal from the
    /// shared queue, could take first, because `observed` leaves it unblocked
    /// (module docs). Unknown masks count as unblocked.
    pub(crate) fn contested_guest_send_claim(
        &self,
        pid: DetPid,
        observed: DetTid,
    ) -> Option<ClaimRefusal> {
        if self.guest_send_claims.is_empty() {
            return None;
        }
        let mask = self.controlled_mask(observed).unwrap_or(0);
        self.guest_send_claims
            .iter()
            .filter(|(thread, claim)| {
                **thread != observed && claim.shared != 0 && self.sigchld_process(**thread) == pid
            })
            .find_map(|(thread, claim)| {
                let pending = self.read_shared_pending_of(pid, *thread)?;
                let contested = claim.shared & pending & !mask;
                (contested != 0).then(|| ClaimRefusal {
                    receiver: *thread,
                    signal: lowest_signal(contested),
                    reason: ClaimReason::Contested { observed },
                })
            })
    }
}
