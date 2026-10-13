/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One `SIGCHLD` per child exit, with Linux's siginfo, on a backend that
//! reports child-exit publication (ptrace), where the kernel sends the parent
//! its own `SIGCHLD` besides the one the scheduler sends at the child's
//! `ChildExit` timer (<https://github.com/rrnewton/hermit/issues/3895>).
//!
//! The scheduler's copy stays process-directed, as before: when the kernel's
//! copy is already pending in the process's shared queue, Linux coalesces
//! the two, and the parent gets one. What this adds:
//!
//! - The exit grant of a child whose notification the scheduler sends
//!   (classified Deliver or Undecided) records the child as *owned* by its
//!   parent, with the siginfo Linux gives the parent. The exiting thread
//!   tells the scheduler its exit status just before its `Exit` request
//!   ([`ChildExitSigchldControl::ExitStatus`]).
//! - The first `SIGCHLD` delivered for an owned child stands for its exit:
//!   the kernel's copy (`si_code` `CLD_EXITED`, `CLD_KILLED` or `CLD_DUMPED`
//!   from the child, [`ChildExitSigchldControl::ClaimKernelCopy`]) or the
//!   scheduler's (`SI_USER` from the tracer,
//!   [`ChildExitSigchldControl::TakeOwnCopy`]). A copy of either kind that
//!   arrives after it is dropped (Detcore's signal hook returns none, or its
//!   filter of unreported signals drops it). Before, the kernel's copy that
//!   arrived after the scheduler's was delivered as a second `SIGCHLD`.
//! - The scheduler's copy, when delivered, gets Linux's siginfo instead of
//!   the tracer's: `CLD_EXITED` and the exit code, the child's pid and real
//!   uid. `si_utime` and `si_stime` are 0 (natively, host CPU time). When the
//!   kernel's copy arrives before the timer, it is the one delivered, and the
//!   timer sends nothing.
//!
//! Which thread takes a process-directed `SIGCHLD`, and when a kernel copy
//! that arrives after the scheduler's interrupts that thread, still depend
//! on host timing, as before; a dropped copy only makes that interruption
//! deliver nothing.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::info;

use super::Scheduler;
use crate::types::DetPid;
use crate::types::DetTid;

/// The parts of a child-exit `SIGCHLD`'s siginfo that Linux fills in, besides
/// `si_signo` and the CPU times.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub struct ChildExitSiginfo {
    /// `CLD_EXITED`, `CLD_KILLED` or `CLD_DUMPED`.
    pub code: i32,
    /// The child's pid.
    pub pid: i32,
    /// The child's real uid.
    pub uid: u32,
    /// The exit code, or the signal that killed the child.
    pub status: i32,
}

impl ChildExitSiginfo {
    /// The raw `siginfo_t` bytes of this `SIGCHLD`.
    pub fn to_bytes(self) -> [u8; 128] {
        let mut bytes = [0; 128];
        bytes[0..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.code.to_ne_bytes());
        // `si_pid`, `si_uid` and `si_status` follow the three ints and their
        // padding, as in `struct { pid_t; uid_t; int; clock_t; clock_t; }`.
        bytes[16..20].copy_from_slice(&self.pid.to_ne_bytes());
        bytes[20..24].copy_from_slice(&self.uid.to_ne_bytes());
        bytes[24..28].copy_from_slice(&self.status.to_ne_bytes());
        bytes
    }
}

/// The `si_code` and `si_pid` of a raw `siginfo_t`.
pub fn siginfo_code_and_pid(bytes: &[u8; 128]) -> (i32, i32) {
    let field = |at: usize| i32::from_ne_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    (field(8), field(16))
}

/// The `si_value` the scheduler's own copy carries when its receiver lives in
/// a nested PID namespace (<https://github.com/rrnewton/hermit/issues/4155>).
/// Linux zeroes `si_pid` for a sender outside the receiver's namespace, so
/// such a receiver cannot tell the tracer's `SI_USER` from an ancestor
/// guest's. The copy is sent instead with `SI_QUEUE` and this value, which no
/// guest can send with a `SIGCHLD` (`is_own_sigchld_mark`).
pub const OWN_SIGCHLD_MARK: i32 = 0x4843_4c44;

/// The raw `siginfo_t` of the scheduler's own copy to a receiver in a nested
/// PID namespace: `SI_QUEUE` from `tracer` (which Linux reports as pid 0
/// there), with [`OWN_SIGCHLD_MARK`].
pub fn marked_own_copy(tracer: u32, uid: u32) -> [u8; 128] {
    let mut bytes = [0; 128];
    bytes[0..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
    bytes[8..12].copy_from_slice(&libc::SI_QUEUE.to_ne_bytes());
    bytes[16..20].copy_from_slice(&tracer.to_ne_bytes());
    bytes[20..24].copy_from_slice(&uid.to_ne_bytes());
    bytes[24..28].copy_from_slice(&OWN_SIGCHLD_MARK.to_ne_bytes());
    bytes
}

/// Whether a `SIGCHLD` with `si_code` `code`, `si_pid` `pid` and `si_int`
/// `value` is the scheduler's own copy: `SI_USER` from `tracer`, the tracer's
/// process, which is never a guest process; or, at a receiver in a nested PID
/// namespace, the marked `SI_QUEUE` copy from outside it (pid 0).
pub fn is_own_sigchld_mark(code: i32, pid: i32, value: i32, tracer: u32) -> bool {
    (code == libc::SI_USER && pid as u32 == tracer)
        || (code == libc::SI_QUEUE && pid == 0 && value == OWN_SIGCHLD_MARK)
}

/// Whether `code` is a child-exit `si_code` (`CLD_EXITED`, `CLD_KILLED`,
/// `CLD_DUMPED`), as opposed to a job-control one or a sender's.
pub fn is_child_exit_code(code: i32) -> bool {
    matches!(code, libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED)
}

/// Which copy a `SIGCHLD` taken from its process's shared queue is, from its
/// siginfo ([`dequeued_copy`]).
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum DequeuedCopy {
    /// The kernel's copy of `child`'s exit notification (a child-exit
    /// `si_code`).
    Kernel { child: DetPid },
    /// A copy the scheduler sent ([`is_own_sigchld_mark`]).
    Scheduler,
    /// Any other copy from the shared queue: a guest's `kill` or queued signal,
    /// or the kernel's job-control notification.
    Other,
    /// A copy whose siginfo Detcore cannot read (`rt_sigtimedwait` with no
    /// info buffer).
    Unknown,
    /// A thread-directed copy (`SI_TKILL`, from `tkill` or `tgkill`), from the
    /// taker's private queue, never the shared queue's.
    Thread,
}

/// Which copy the `SIGCHLD` with siginfo `info` is.
pub fn dequeued_copy(info: &[u8; 128], tracer: u32) -> Option<DequeuedCopy> {
    let (code, pid) = siginfo_code_and_pid(info);
    let value = i32::from_ne_bytes(info[24..28].try_into().expect("four bytes"));
    Some(if is_child_exit_code(code) {
        DequeuedCopy::Kernel {
            child: DetPid::from_raw(pid),
        }
    } else if is_own_sigchld_mark(code, pid, value, tracer) {
        DequeuedCopy::Scheduler
    } else if code == libc::SI_TKILL {
        DequeuedCopy::Thread
    } else {
        DequeuedCopy::Other
    })
}

/// The control message for taking the `SIGCHLD` with siginfo `info`
/// ([`ChildExitSigchldControl::Dequeue`]). `admits`: the thread took it at a
/// delivery stop, which an `InboundSignal` request follows if the copy is
/// kept.
pub fn control_for(info: &[u8; 128], tracer: u32, admits: bool) -> Option<ChildExitSigchldControl> {
    Some(ChildExitSigchldControl::Dequeue {
        copy: dequeued_copy(info, tracer)?,
        admits,
    })
}

/// A message about child-exit `SIGCHLD`s from a guest thread. Like the
/// SIGALRM ledger's, it carries no logical time and its answer carries none
/// back.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum ChildExitSigchldControl {
    /// The calling thread is about to request the exit of its whole process,
    /// whose parent's `SIGCHLD` would carry `code`, `status` and `uid`. Sent
    /// in the thread's own turn, just before its `Exit` request.
    ExitStatus { code: i32, status: i32, uid: u32 },
    /// A `SIGCHLD` with a child-exit `si_code` from `child` stopped the calling
    /// thread. Answers whether it is dropped: a copy for an owned child whose
    /// exit a `SIGCHLD` was already delivered for.
    ClaimKernelCopy { child: DetPid },
    /// A `SIGCHLD` the scheduler sent (`SI_USER` from the tracer) stopped the
    /// calling thread. Answers what becomes of it ([`OwnCopy`]).
    TakeOwnCopy,
    /// The calling thread took the one `SIGCHLD` pending in its process's
    /// shared queue, which is `copy` by its siginfo: at a delivery stop
    /// (`admits`), or by `rt_sigtimedwait`, a signalfd read, or the backend
    /// passing it on unreported. Answers what becomes of it, as
    /// `ClaimKernelCopy` and `TakeOwnCopy` do, unless it holds a copy the
    /// scheduler sent since the last such dequeue (the send window,
    /// [`ChildExitSigchldLedger::take_window`]). Appended after `TakeOwnCopy`,
    /// so that every earlier variant's tag is unchanged.
    Dequeue { copy: DequeuedCopy, admits: bool },
    /// The calling thread's process just set `SIGCHLD` to `SIG_IGN` or
    /// `SIG_DFL`, in its own turn, and Linux flushed the pending `SIGCHLD`
    /// (`do_sigaction`). Appended after `Dequeue`.
    Flushed,
    /// Whether a `SIGCHLD` the scheduler sent is still pending in the calling
    /// thread's process's shared queue, as far as the schedule fixes it
    /// (`Scheduler::committed_sigchld_pending`), asked by a sleep woken for it,
    /// or holding a private one, in its own turn. Appended after `Flushed`.
    PlacedPending,
    /// The calling thread, in its own turn, sent `target` a thread-directed
    /// `SIGCHLD` (`tkill`, `tgkill`, `rt_tgsigqueueinfo`): a copy in
    /// `target`'s private queue ([`ChildExitSigchldLedger::private_sent`]).
    /// Appended after `PlacedPending`.
    PrivateSent { target: DetTid },
    /// The calling thread, in its own turn, sent process `target` a
    /// process-directed `SIGCHLD` (`kill`, `rt_sigqueueinfo`): a committed
    /// copy in `target`'s shared queue, entered in its send window
    /// ([`WindowSend::Guest`]). Appended after `PrivateSent`.
    SharedSent { target: DetPid },
    /// The calling thread's `rt_sigtimedwait` failed with EFAULT, which may
    /// have taken a signal before its siginfo copy-out faulted; whether
    /// `SIGCHLD` is still pending in its private and its process's shared
    /// queue says whether the wait took a recorded private copy or the
    /// window's. Appended after `SharedSent`.
    WaitFaulted {
        private_pending: bool,
        shared_pending: bool,
    },
    /// A guest's send failed after `PrivateSent` (when it made a new record)
    /// or `SharedSent` recorded its copy: undo that record. Appended after
    /// `WaitFaulted`.
    SendFailed {
        private: Option<DetTid>,
        shared: Option<DetPid>,
    },
}

/// What becomes of a `SIGCHLD` the scheduler sent, at its delivery stop.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum OwnCopy {
    /// It stands for this child's exit and is delivered with this siginfo.
    Deliver(ChildExitSiginfo),
    /// The kernel's copy of that exit was delivered first: it is dropped.
    Duplicate,
    /// No owned child's copy is outstanding: it is delivered as it is.
    Unowned,
}

/// A committed `SIGCHLD` send to a process's shared queue, in the child-exit
/// ledger's send window.
#[derive(PartialEq, Debug, Eq, Clone, Copy)]
pub(crate) enum WindowSend {
    /// The scheduler's copy for this owned child's `ChildExit` timer.
    Child(DetPid),
    /// Any other copy the scheduler sent.
    Scheduler,
    /// A guest's process-directed copy, sent in its committed turn.
    Guest,
}

/// The answer to a [`ChildExitSigchldControl`].
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum ChildExitSigchldAnswer {
    Recorded,
    Claimed(bool),
    OwnCopy(OwnCopy),
    /// The kernel's copy is delivered, with this siginfo: it stands for
    /// another member of its child's SIGKILL cohort.
    ClaimedFor(ChildExitSiginfo),
    /// The answer to [`ChildExitSigchldControl::PlacedPending`]. Appended
    /// after `ClaimedFor`.
    Placed(bool),
}

/// Where an owned child's notification stands.
#[derive(PartialEq, Debug, Eq, Clone, Copy)]
enum Notification {
    /// The `ChildExit` timer has not fired.
    Unsent(ChildExitSiginfo),
    /// The scheduler sent its copy, which has not been delivered.
    Sent(ChildExitSiginfo),
    /// The kernel's copy was delivered before the timer: the timer sends
    /// nothing.
    DeliveredBeforeSend,
    /// The kernel's copy was delivered while the scheduler's was sent and not
    /// delivered: if the scheduler's arrives, it is dropped (it may also have
    /// coalesced into the kernel's).
    DeliveredWhileSent,
    /// A `SIGCHLD` for this exit was delivered: a kernel copy that arrives
    /// later is dropped.
    Delivered,
}

/// The scheduler's record of the child-exit `SIGCHLD`s it sends itself.
#[derive(Debug, Default)]
pub(crate) struct ChildExitSigchldLedger {
    /// The exit status each exiting process reported, until its `Exit` grant.
    pub(super) exit_status: BTreeMap<DetPid, ChildExitSiginfo>,
    /// For each parent process, its owned children's notifications.
    owned: BTreeMap<DetPid, BTreeMap<DetPid, Notification>>,
    /// For each parent process, the owned children whose copy the scheduler
    /// sent, in the order sent, until a `SIGCHLD` it sent is matched to them.
    sent: BTreeMap<DetPid, VecDeque<DetPid>>,
    /// The owned children killed by one SIGKILL send of several processes,
    /// by that send's token ([`ChildExitSigchldLedger::claim_for_cohort`]).
    cohorts: BTreeMap<DetPid, u64>,
    /// For each parent process, the committed `SIGCHLD` sends to its shared
    /// queue since a thread of the process last took the one pending there,
    /// in order ([`WindowSend`]). Linux does not queue a standard signal
    /// already pending (`legacy_queue`), so every send in the window, and any
    /// kernel copy that was already pending, is one pending signal, delivered
    /// once, with the siginfo of whichever copy was queued first
    /// (<https://github.com/rrnewton/hermit/issues/4126>).
    window: BTreeMap<DetPid, Vec<WindowSend>>,
    /// The threads whose taken `SIGCHLD` this ledger matched to an owned
    /// child's exit, until their `InboundSignal` request is granted: the
    /// `ChildExit` timer does not let such a deferred delivery stand in for
    /// another child (`Scheduler::deferred_child_exit_sigchld`).
    pub(super) attributed: std::collections::BTreeSet<DetTid>,
    /// The threads a guest sent a thread-directed `SIGCHLD` that is still in
    /// their private queue, recorded in the sender's turn. Linux takes the
    /// private queue first (`dequeue_signal`), so such a thread's next
    /// `SIGCHLD` dequeue is that copy, whatever its siginfo says: a guest's
    /// `rt_tgsigqueueinfo` may carry `SI_QUEUE`, `SI_USER` or even a child-exit
    /// code. It is not the shared queue's, so it leaves the send window and the
    /// ledger alone (the design check of
    /// <https://github.com/rrnewton/hermit/issues/4132>).
    pub(super) private_sent: std::collections::BTreeSet<DetTid>,
    /// The processes one of whose threads entered a blocking call outside the
    /// schedule (external I/O or `sigsuspend`) with a mask that is unknown or
    /// leaves SIGCHLD unblocked, recorded at that grant's commit point. Such a
    /// thread could take the pending SIGCHLD at a moment host timing chooses,
    /// and its dequeue reaches the ledger only after it rejoins, so the send
    /// window could match it to a send queued after it; for such a process
    /// the window is not used (`Scheduler::sigchld_window_usable`).
    pub(super) outside_takers: std::collections::BTreeSet<DetPid>,
}

/// What the `ChildExit` timer of an owned child does.
#[derive(PartialEq, Debug, Eq, Clone, Copy)]
pub(crate) enum TimerSend {
    /// Send the scheduler's copy.
    Send,
    /// Send nothing: the kernel's copy was delivered first.
    Nothing,
}

impl ChildExitSigchldLedger {
    /// Make `child`'s notification to `parent` the scheduler's own, if
    /// `child` reported its exit status. Returns whether it did.
    pub(crate) fn own(&mut self, parent: DetPid, child: DetPid) -> bool {
        let Some(info) = self.exit_status.remove(&child) else {
            return false;
        };
        self.owned
            .entry(parent)
            .or_default()
            .insert(child, Notification::Unsent(info));
        true
    }

    /// Forget the exit status `process` reported, at its `Exit` grant.
    pub(crate) fn forget_exit_status(&mut self, process: DetPid) {
        self.exit_status.remove(&process);
    }

    fn state(&mut self, parent: DetPid, child: DetPid) -> Option<&mut Notification> {
        self.owned.get_mut(&parent)?.get_mut(&child)
    }

    fn forget(&mut self, parent: DetPid, child: DetPid) {
        self.cohorts.remove(&child);
        if let Some(children) = self.owned.get_mut(&parent) {
            children.remove(&child);
            if children.is_empty() {
                self.owned.remove(&parent);
            }
        }
    }

    /// What the `ChildExit` timer of `child` does, or `None` when the
    /// notification is not the scheduler's own.
    pub(crate) fn timer(&mut self, parent: DetPid, child: DetPid) -> Option<TimerSend> {
        match *self.state(parent, child)? {
            Notification::Unsent(info) => {
                *self.state(parent, child)? = Notification::Sent(info);
                self.sent.entry(parent).or_default().push_back(child);
                Some(TimerSend::Send)
            }
            Notification::DeliveredBeforeSend => {
                self.forget(parent, child);
                Some(TimerSend::Nothing)
            }
            _ => None,
        }
    }

    /// The `ChildExit` timer of `child` committed a `SIGCHLD` delivery that a
    /// thread had already taken and deferred, which stands for this exit: a
    /// kernel copy of this child's that arrives later is dropped.
    pub(crate) fn stand_in(&mut self, parent: DetPid, child: DetPid) {
        if let Some(state) = self.state(parent, child) {
            *state = Notification::Delivered;
        }
    }

    /// Make owned `child` a member of the SIGKILL cohort `cohort`: the
    /// processes one send killed together.
    pub(crate) fn join_cohort(&mut self, child: DetPid, cohort: u64) {
        self.cohorts.insert(child, cohort);
    }

    /// The kernel's copy naming `child`, a member of a SIGKILL cohort. Its
    /// victims' exits were all published before the send returned, so the
    /// kernel coalesced them into one pending `SIGCHLD`, naming whichever it
    /// published first: a host-chosen member. The copy is delivered instead
    /// for the cohort's lowest-pid member for which no `SIGCHLD` was delivered
    /// yet, with that member's siginfo, as Linux could have published it
    /// first. The others are notified by their own timers. Returns that
    /// siginfo, or `None` (`child` is in no cohort, or every member's exit was
    /// delivered for), leaving the copy to `claim`.
    fn claim_for_cohort(&mut self, parent: DetPid, child: DetPid) -> Option<ChildExitSiginfo> {
        let cohort = *self.cohorts.get(&child)?;
        let (member, info) = self
            .owned
            .get(&parent)?
            .iter()
            .filter(|(member, _)| self.cohorts.get(member) == Some(&cohort))
            .find_map(|(member, state)| match *state {
                Notification::Unsent(info) | Notification::Sent(info) => Some((*member, info)),
                _ => None,
            })?;
        let state = self.state(parent, member)?;
        *state = match *state {
            Notification::Unsent(_) => Notification::DeliveredBeforeSend,
            _ => Notification::DeliveredWhileSent,
        };
        Some(info)
    }

    /// Whether the kernel's copy of `child`'s notification to `parent` is
    /// dropped, because a `SIGCHLD` for that exit was delivered already. The
    /// first one is delivered.
    fn claim(&mut self, parent: DetPid, child: DetPid) -> bool {
        let Some(state) = self.state(parent, child) else {
            return false;
        };
        match *state {
            Notification::Unsent(_) => {
                *state = Notification::DeliveredBeforeSend;
                false
            }
            Notification::Sent(_) => {
                *state = Notification::DeliveredWhileSent;
                false
            }
            Notification::Delivered => {
                self.forget(parent, child);
                true
            }
            Notification::DeliveredBeforeSend | Notification::DeliveredWhileSent => false,
        }
    }

    /// What becomes of a `SIGCHLD` the scheduler sent to `parent`, at its
    /// delivery stop: it stands for the oldest owned child whose copy was sent
    /// and whose exit no `SIGCHLD` was delivered for yet; failing that, it is
    /// a copy whose child's kernel copy went first; failing that, it is not
    /// an owned child's.
    fn take_own(&mut self, parent: DetPid) -> OwnCopy {
        let Some(order) = self.sent.get(&parent) else {
            return OwnCopy::Unowned;
        };
        let children = self.owned.get(&parent);
        let state = |child: &DetPid| children.and_then(|children| children.get(child)).copied();
        let pick = |wanted: fn(Notification) -> bool| {
            order
                .iter()
                .position(|child| state(child).is_some_and(wanted))
        };
        if let Some(at) = pick(|state| matches!(state, Notification::Sent(_))) {
            let child = self.sent.get_mut(&parent).unwrap().remove(at).unwrap();
            let Some(Notification::Sent(info)) = self.state(parent, child).copied() else {
                unreachable!("picked a sent child");
            };
            *self.state(parent, child).unwrap() = Notification::Delivered;
            self.tidy_sent(parent);
            return OwnCopy::Deliver(info);
        }
        if let Some(at) = pick(|state| matches!(state, Notification::DeliveredWhileSent)) {
            let child = self.sent.get_mut(&parent).unwrap().remove(at).unwrap();
            self.forget(parent, child);
            self.tidy_sent(parent);
            return OwnCopy::Duplicate;
        }
        OwnCopy::Unowned
    }

    fn tidy_sent(&mut self, parent: DetPid) {
        if self.sent.get(&parent).is_some_and(VecDeque::is_empty) {
            self.sent.remove(&parent);
        }
    }

    /// Whether `child`'s notification to `parent` is one this ledger owns.
    fn owns(&self, parent: DetPid, child: DetPid) -> bool {
        self.owned
            .get(&parent)
            .is_some_and(|children| children.contains_key(&child))
    }

    /// The scheduler sent `parent` a `SIGCHLD`: `child`'s timer copy, or
    /// `None` for any other.
    pub(crate) fn note_send(&mut self, parent: DetPid, child: Option<DetPid>) {
        let send = child.map_or(WindowSend::Scheduler, WindowSend::Child);
        self.window.entry(parent).or_default().push(send);
    }

    /// A guest sent `parent` a process-directed `SIGCHLD`, in its committed
    /// turn: it joins the window like a scheduler send.
    pub(crate) fn note_guest_send(&mut self, parent: DetPid) {
        self.window
            .entry(parent)
            .or_default()
            .push(WindowSend::Guest);
    }

    /// The guest's send `note_guest_send` recorded for `parent` failed: drop
    /// its entry, the last guest one, entered in the same turn.
    pub(crate) fn unnote_guest_send(&mut self, parent: DetPid) {
        if let Some(sends) = self.window.get_mut(&parent)
            && let Some(at) = sends.iter().rposition(|send| *send == WindowSend::Guest)
        {
            sends.remove(at);
        }
    }

    /// Whether a `SIGCHLD` the scheduler sent `parent` is pending in its shared
    /// queue: a send since the last dequeue.
    pub(crate) fn window_open(&self, parent: DetPid) -> bool {
        self.window
            .get(&parent)
            .is_some_and(|sends| !sends.is_empty())
    }

    /// Mark every owned child sent in `sends` as notified: its copy was in the
    /// one pending signal just delivered, or flushed with it. Returns the
    /// siginfo of the oldest whose copy had not yet been matched.
    fn settle_sends(&mut self, parent: DetPid, sends: &[WindowSend]) -> Option<ChildExitSiginfo> {
        let mut oldest = None;
        for child in sends.iter().filter_map(|send| match send {
            WindowSend::Child(child) => Some(child),
            _ => None,
        }) {
            let Some(state) = self.state(parent, *child).copied() else {
                continue;
            };
            match state {
                Notification::Sent(info) => {
                    oldest.get_or_insert(info);
                    *self.state(parent, *child).unwrap() = Notification::Delivered;
                }
                Notification::DeliveredWhileSent => self.forget(parent, *child),
                _ => continue,
            }
            if let Some(order) = self.sent.get_mut(&parent) {
                order.retain(|sent| sent != child);
            }
        }
        self.tidy_sent(parent);
        oldest
    }

    /// A thread of `parent` took the one `SIGCHLD` pending in its shared
    /// queue. If the scheduler sent one since the last such dequeue, that
    /// signal holds every send in the window, whatever its siginfo says (a
    /// copy queued first keeps its siginfo, and a later one is not queued), so
    /// it stands for every owned child sent there and is never dropped: it is
    /// delivered with the oldest one's siginfo (`Some(Some(..))`), or as it is
    /// if none is owned (`Some(None)`). A kernel copy it absorbed, `absorbed`,
    /// of a child already notified is forgotten. `None` when the window is
    /// empty, and the siginfo decides as before.
    pub(crate) fn take_window(
        &mut self,
        parent: DetPid,
        absorbed: Option<DetPid>,
    ) -> Option<Option<ChildExitSiginfo>> {
        let sends = self
            .window
            .remove(&parent)
            .filter(|sends| !sends.is_empty())?;
        let deliver = self.settle_sends(parent, &sends);
        // A guest's copy queued first keeps its siginfo, as Linux keeps the
        // first-queued copy's (the Claude re-check of
        // <https://github.com/rrnewton/hermit/pull/4131>).
        let deliver = if sends.first() == Some(&WindowSend::Guest) {
            None
        } else {
            deliver
        };
        // An absorbed kernel copy of a child already notified is forgotten: no
        // later copy of its can come. A child not yet notified keeps its own
        // timer, so its notification does not depend on whether host timing
        // posted its kernel copy before this send (absorbed here) or after
        // (coalesced into it). A SIGKILL cohort's copy names a host-chosen
        // member and is its members' only one: nothing is forgotten for it.
        if let Some(child) = absorbed
            && !sends.contains(&WindowSend::Child(child))
            && !self.cohorts.contains_key(&child)
            && matches!(self.state(parent, child), Some(Notification::Delivered))
        {
            self.forget(parent, child);
        }
        Some(deliver)
    }

    /// `parent` set `SIGCHLD` to `SIG_IGN` or `SIG_DFL`, and Linux flushed its
    /// pending `SIGCHLD`: the sends in the window were discarded with it, and
    /// a kernel copy of those children's that lands later is dropped.
    pub(crate) fn flush(&mut self, parent: DetPid) {
        if let Some(sends) = self.window.remove(&parent) {
            self.settle_sends(parent, &sends);
        }
    }

    /// Forget `parent`'s window at its death, which discards its pending
    /// signals.
    pub(crate) fn forget_window(&mut self, parent: DetPid) {
        self.window.remove(&parent);
    }

    /// Forget what this ledger keeps for `process` at its death: its window
    /// and whether a thread of it could take SIGCHLD outside the schedule.
    pub(crate) fn forget_process(&mut self, process: DetPid) {
        self.forget_window(process);
        self.outside_takers.remove(&process);
    }
}

impl Scheduler {
    /// At the `ChildExit` timer of `child`, whose notification to `parent`
    /// is the scheduler's own (`ChildExitSigchldLedger::own`): send its
    /// `SIGCHLD` to `target`, process-directed as for any other child, unless
    /// the kernel's copy was delivered first. Returns false, doing nothing,
    /// for a notification that is not the scheduler's own.
    pub(super) fn send_child_exit_sigchld(
        &mut self,
        parent: DetPid,
        target: DetTid,
        child: DetPid,
    ) -> bool {
        match self.child_exit_sigchld.timer(parent, child) {
            None => false,
            Some(TimerSend::Nothing) => {
                info!(
                    "[dpid {}] SIGCHLD for child {} was the kernel's, delivered before the timer.",
                    parent, child
                );
                true
            }
            Some(TimerSend::Send) => {
                // A receiver in a nested PID namespace sees the tracer as pid
                // 0, as it sees any sender outside it: its copy is marked.
                let marked = self
                    .thread_tree
                    .pid_namespaces
                    .namespace_of(DetTid::from_raw(parent.as_raw()))
                    .is_some()
                    .then(|| marked_own_copy(std::process::id(), nix::unistd::getuid().as_raw()));
                self.sending_child_exit = Some(child);
                self.fire_alarm_with(parent, target, nix::sys::signal::Signal::SIGCHLD, marked);
                self.sending_child_exit = None;
                true
            }
        }
    }

    /// Answer a [`ChildExitSigchldControl`] from `dtid`.
    /// [`Self::child_exit_sigchld_control`] for a receiver `dtid` that may
    /// live in a nested PID namespace (https://github.com/rrnewton/hermit/issues/4155).
    /// The kernel writes `si_pid` in the receiver's namespace, so a claim of
    /// the kernel's copy names the child by that pid: it is translated first,
    /// and a pid that names no task there claims nothing (the copy is
    /// delivered as it is). The siginfo of the scheduler's own copy names the
    /// child by its DetPid, which is localized for the receiver. Neither
    /// fallback below is reachable from a guest: an untranslatable receiver's
    /// first fork is refused at the clone return, so it has no child, and a
    /// child lives in its receiver's namespace or a deeper one, so it is
    /// always visible there.
    pub(crate) fn child_exit_sigchld_control_for_receiver(
        &mut self,
        dtid: DetTid,
        control: ChildExitSigchldControl,
    ) -> ChildExitSigchldAnswer {
        let Some(namespace) = self.thread_tree.pid_namespaces.namespace_of(dtid) else {
            return self.child_exit_sigchld_control(dtid, control);
        };
        let control = match control {
            ChildExitSigchldControl::ClaimKernelCopy { child } => {
                match self
                    .thread_tree
                    .pid_namespaces
                    .resolve(Some(namespace), child.as_raw())
                {
                    Some(task) => ChildExitSigchldControl::ClaimKernelCopy { child: task },
                    None => return ChildExitSigchldAnswer::Claimed(false),
                }
            }
            // A dequeue of the kernel's copy names the child the same way; a
            // pid that names no task there is taken as another copy.
            ChildExitSigchldControl::Dequeue {
                copy: DequeuedCopy::Kernel { child },
                admits,
            } => ChildExitSigchldControl::Dequeue {
                copy: match self
                    .thread_tree
                    .pid_namespaces
                    .resolve(Some(namespace), child.as_raw())
                {
                    Some(task) => DequeuedCopy::Kernel { child: task },
                    None => DequeuedCopy::Other,
                },
                admits,
            },
            other => other,
        };
        let localize = |scheduler: &Self, mut info: ChildExitSiginfo| {
            if let Some(local) = scheduler
                .thread_tree
                .pid_namespaces
                .localize(Some(namespace), DetTid::from_raw(info.pid))
            {
                info.pid = local;
            }
            info
        };
        match self.child_exit_sigchld_control(dtid, control) {
            ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(info)) => {
                ChildExitSigchldAnswer::OwnCopy(OwnCopy::Deliver(localize(self, info)))
            }
            ChildExitSigchldAnswer::ClaimedFor(info) => {
                ChildExitSigchldAnswer::ClaimedFor(localize(self, info))
            }
            answer => answer,
        }
    }

    pub(crate) fn child_exit_sigchld_control(
        &mut self,
        dtid: DetTid,
        control: ChildExitSigchldControl,
    ) -> ChildExitSigchldAnswer {
        match control {
            ChildExitSigchldControl::ExitStatus { code, status, uid } => {
                let process = self.sigchld_process(dtid);
                self.child_exit_sigchld.exit_status.insert(
                    process,
                    ChildExitSiginfo {
                        code,
                        pid: process.as_raw(),
                        uid,
                        status,
                    },
                );
                ChildExitSigchldAnswer::Recorded
            }
            ChildExitSigchldControl::ClaimKernelCopy { child } => {
                self.claim_kernel_copy(dtid, child)
            }
            ChildExitSigchldControl::TakeOwnCopy => {
                ChildExitSigchldAnswer::OwnCopy(self.take_own_copy(dtid))
            }
            ChildExitSigchldControl::Dequeue { copy, admits } => {
                self.dequeue_sigchld(dtid, copy, admits)
            }
            ChildExitSigchldControl::Flushed => {
                let parent = self.sigchld_process(dtid);
                self.child_exit_sigchld.flush(parent);
                // `do_sigaction` flushes every thread's private queue too.
                let private = std::mem::take(&mut self.child_exit_sigchld.private_sent);
                self.child_exit_sigchld.private_sent = private
                    .into_iter()
                    .filter(|thread| self.sigchld_process(*thread) != parent)
                    .collect();
                ChildExitSigchldAnswer::Recorded
            }
            ChildExitSigchldControl::PlacedPending => {
                ChildExitSigchldAnswer::Placed(self.committed_sigchld_pending(dtid))
            }
            ChildExitSigchldControl::PrivateSent { target } => {
                // Whether this made a new record, which a failed send undoes.
                ChildExitSigchldAnswer::Placed(
                    self.models_signal_targets
                        && self.child_exit_sigchld.private_sent.insert(target),
                )
            }
            ChildExitSigchldControl::SendFailed { private, shared } => {
                if let Some(target) = private {
                    self.child_exit_sigchld.private_sent.remove(&target);
                }
                if let Some(target) = shared {
                    self.child_exit_sigchld.unnote_guest_send(target);
                }
                ChildExitSigchldAnswer::Recorded
            }
            ChildExitSigchldControl::SharedSent { target } => {
                if self.models_signal_targets && self.sigchld_window_usable(target) {
                    self.child_exit_sigchld.note_guest_send(target);
                }
                ChildExitSigchldAnswer::Recorded
            }
            ChildExitSigchldControl::WaitFaulted {
                private_pending,
                shared_pending,
            } => {
                let parent = self.sigchld_process(dtid);
                // The private queue is taken first.
                if self.child_exit_sigchld.private_sent.contains(&dtid) {
                    if !private_pending {
                        self.child_exit_sigchld.private_sent.remove(&dtid);
                    }
                } else if !shared_pending && self.sigchld_window_usable(parent) {
                    self.child_exit_sigchld.take_window(parent, None);
                }
                ChildExitSigchldAnswer::Recorded
            }
        }
    }

    fn claim_kernel_copy(&mut self, dtid: DetTid, child: DetPid) -> ChildExitSigchldAnswer {
        let parent = self.sigchld_process(dtid);
        if let Some(info) = self.child_exit_sigchld.claim_for_cohort(parent, child) {
            debug!(
                "[dtid {}] the kernel's SIGCHLD for child {} stands for child {}, its cohort's first",
                dtid, child, info.pid
            );
            return ChildExitSigchldAnswer::ClaimedFor(info);
        }
        let claimed = self.child_exit_sigchld.claim(parent, child);
        if claimed {
            // Not an INFO record: whether the kernel's copy arrives at
            // all depends on host timing (it can coalesce with another
            // SIGCHLD in the shared queue).
            debug!(
                "[dtid {}] dropping the kernel's SIGCHLD for child {}: one was delivered for that exit",
                dtid, child
            );
        }
        ChildExitSigchldAnswer::Claimed(claimed)
    }

    fn take_own_copy(&mut self, dtid: DetTid) -> OwnCopy {
        let parent = self.sigchld_process(dtid);
        let own = self.child_exit_sigchld.take_own(parent);
        if own == OwnCopy::Duplicate {
            debug!(
                "[dtid {}] dropping the scheduler's SIGCHLD: the kernel's was delivered first",
                dtid
            );
        }
        own
    }

    /// `dtid` took the one `SIGCHLD` pending in its process's shared queue,
    /// `copy` by its siginfo ([`ChildExitSigchldControl::Dequeue`]). Where the
    /// scheduler models signal targets and sent one since the last dequeue,
    /// the signal stands for the send window
    /// ([`ChildExitSigchldLedger::take_window`]); at a delivery stop
    /// (`admits`) it is then one the scheduler placed at a committed point,
    /// granted at its turn (`sigchld_ready` for `dtid`) rather than deferred
    /// behind runnable work. A mark is made only by the dequeue it admits, so
    /// no delivery can take another's: one mark for the process, which two
    /// child exits shared, let a sibling's delivery of the first copy take the
    /// mark the sleeper's later copy needed (the Claude re-check of
    /// <https://github.com/rrnewton/hermit/pull/4131>). With no send in the
    /// window the siginfo decides, as before, and nothing is marked.
    fn dequeue_sigchld(
        &mut self,
        dtid: DetTid,
        copy: DequeuedCopy,
        admits: bool,
    ) -> ChildExitSigchldAnswer {
        let parent = self.sigchld_process(dtid);
        self.child_exit_sigchld.attributed.remove(&dtid);
        // A guest's thread-directed copy, taken first from the private queue:
        // delivered as it is, granted at its turn (it was queued at the
        // sender's committed turn), and not the shared queue's.
        if self.child_exit_sigchld.private_sent.remove(&dtid) {
            if admits {
                self.blocked.sigchld_ready.insert(dtid);
            }
            return ChildExitSigchldAnswer::Recorded;
        }
        let absorbed = match copy {
            DequeuedCopy::Kernel { child } => Some(child),
            _ => None,
        };
        // Where a thread outside the schedule could take SIGCHLD, the window
        // cannot tell which sends a dequeue held: the siginfo decides, as
        // before the window existed.
        if !self.sigchld_window_usable(parent) {
            self.child_exit_sigchld.forget_window(parent);
        }
        // A thread-directed copy no sender recorded (a backend without
        // signal-target modelling) is still not the shared queue's.
        if self.models_signal_targets
            && copy != DequeuedCopy::Thread
            && let Some(deliver) = self.child_exit_sigchld.take_window(parent, absorbed)
        {
            if admits {
                self.blocked.sigchld_ready.insert(dtid);
                self.child_exit_sigchld.attributed.insert(dtid);
            }
            return ChildExitSigchldAnswer::OwnCopy(match deliver {
                Some(info) => OwnCopy::Deliver(info),
                None => OwnCopy::Unowned,
            });
        }
        let (answer, attributed) = match copy {
            DequeuedCopy::Kernel { child } => {
                let owned = self.child_exit_sigchld.owns(parent, child);
                let answer = self.claim_kernel_copy(dtid, child);
                let kept = !matches!(answer, ChildExitSigchldAnswer::Claimed(true));
                (answer, owned && kept)
            }
            DequeuedCopy::Scheduler => {
                let own = self.take_own_copy(dtid);
                (
                    ChildExitSigchldAnswer::OwnCopy(own),
                    matches!(own, OwnCopy::Deliver(_)),
                )
            }
            DequeuedCopy::Other | DequeuedCopy::Unknown | DequeuedCopy::Thread => {
                (ChildExitSigchldAnswer::Recorded, false)
            }
        };
        if admits && attributed {
            self.child_exit_sigchld.attributed.insert(dtid);
        }
        answer
    }

    /// Whether a committed `SIGCHLD` is pending for `dtid` to take in its own
    /// turn: a guest's thread-directed copy in its private queue
    /// (`private_sent`), which no other thread can take; or a send in its
    /// process's window, which is used only where no thread outside the
    /// schedule could take that one at a moment host timing chooses
    /// (`sigchld_window_usable`). Without signal-target modelling, the sleep
    /// decides from `/proc` as before.
    ///
    /// Otherwise the answer is no and the sleep goes on to its deadline: which
    /// thread takes the shared copy is then a matter of host timing, and Linux
    /// allows either, so the woken sleep never answers EINTR for a copy
    /// another thread may take. In a process that does not use the window
    /// the woken path thus never answers EINTR for a shared copy; the signal
    /// ends the sleep only by its delivery, as before the queued wake existed
    /// (deciding from the mask and dispositions there answered EINTR while
    /// the handler ran in a `sigsuspend` sibling, the Claude re-check of
    /// <https://github.com/rrnewton/hermit/pull/4131> at ba6747e8). A private
    /// copy only this thread can take counts there too.
    pub(crate) fn committed_sigchld_pending(&self, dtid: DetTid) -> bool {
        if !self.models_signal_targets {
            return true;
        }
        if self.child_exit_sigchld.private_sent.contains(&dtid) {
            return true;
        }
        let parent = self.sigchld_process(dtid);
        self.child_exit_sigchld.window_open(parent) && self.sigchld_window_usable(parent)
    }

    /// Whether the send window may attribute `parent`'s shared SIGCHLD:
    /// every thread that could take it does so at a committed point. Not
    /// where one of its threads ever entered a blocking call outside the
    /// schedule that could take it (`outside_takers`,
    /// `note_outside_blocker`). Such a thread's dequeue reaches the ledger
    /// only once it rejoins, after sends it never held may have joined the
    /// window, so a window there delivered one SIGCHLD twice; such a process
    /// keeps the behaviour from before the window, where the siginfo decides
    /// (the Claude re-check of
    /// <https://github.com/rrnewton/hermit/pull/4131> at f37277ef). A
    /// signalfd read in the schedule is a committed dequeue, which the window
    /// orders like any other. The condition is set at commit points and only
    /// ever becomes true, so the answer is a function of the schedule.
    pub(crate) fn sigchld_window_usable(&self, parent: DetPid) -> bool {
        !self.child_exit_sigchld.outside_takers.contains(&parent)
    }

    /// `thread` was granted a blocking call outside the schedule, sleeping
    /// with `mask` (`None` if unknown): if it could take SIGCHLD there, its
    /// process no longer uses the send window (`sigchld_window_usable`). It
    /// could if the mask leaves SIGCHLD unblocked, or, whatever the mask, if
    /// the call is an I/O one (not `sigsuspend`, `descriptors` false) of a
    /// process holding a signalfd for SIGCHLD (a fork inherits the record):
    /// the call may be a read of it.
    pub(crate) fn note_outside_blocker(
        &mut self,
        thread: DetTid,
        mask: Option<u64>,
        descriptors: bool,
    ) {
        let sigchld = super::kernel_signal_bit(libc::SIGCHLD);
        let process = self.sigchld_process(thread);
        let reads_signalfd = descriptors
            && self
                .signalfd_masks
                .get(&process)
                .is_some_and(|mask| mask & sigchld != 0);
        if self.models_signal_targets
            && (reads_signalfd || mask.is_none_or(|mask| mask & sigchld == 0))
            && self.child_exit_sigchld.outside_takers.insert(process)
        {
            debug!(
                "[dtid {}] process {} no longer uses the SIGCHLD send window: a thread may take SIGCHLD outside the schedule",
                thread, process
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: DetPid = DetPid::from_raw(10);
    const CHILD: DetPid = DetPid::from_raw(20);

    fn exited(pid: i32, status: i32) -> ChildExitSiginfo {
        ChildExitSiginfo {
            code: libc::CLD_EXITED,
            pid,
            uid: 1000,
            status,
        }
    }

    fn owned_child() -> ChildExitSigchldLedger {
        let mut ledger = ChildExitSigchldLedger::default();
        ledger.exit_status.insert(CHILD, exited(CHILD.as_raw(), 3));
        assert!(ledger.own(PARENT, CHILD));
        ledger
    }

    /// The bytes are Linux's `siginfo_t` layout for `SIGCHLD`: the fields
    /// read back through libc's accessors.
    #[test]
    fn child_exit_siginfo_has_linuxs_layout() {
        let bytes = exited(42, 7).to_bytes();
        // SAFETY: a `siginfo_t` is 128 plain bytes.
        let info: libc::siginfo_t = unsafe { std::mem::transmute(bytes) };
        assert_eq!(info.si_signo, libc::SIGCHLD);
        assert_eq!(info.si_code, libc::CLD_EXITED);
        // SAFETY: a `SIGCHLD`'s siginfo holds the child fields.
        unsafe {
            assert_eq!(info.si_pid(), 42);
            assert_eq!(info.si_uid(), 1000);
            assert_eq!(info.si_status(), 7);
            assert_eq!(info.si_utime(), 0);
            assert_eq!(info.si_stime(), 0);
        }
        assert_eq!(siginfo_code_and_pid(&bytes), (libc::CLD_EXITED, 42));
    }

    /// Only a child that reported its status is owned, and only by its
    /// parent.
    #[test]
    fn a_child_is_owned_once_it_reported_its_status() {
        let mut ledger = ChildExitSigchldLedger::default();
        assert!(!ledger.own(PARENT, CHILD), "no status reported");
        assert_eq!(ledger.timer(PARENT, CHILD), None);
        let ledger = &mut owned_child();
        assert!(
            !ledger.claim(DetPid::from_raw(11), CHILD),
            "another process"
        );
        assert!(!ledger.claim(PARENT, DetPid::from_raw(21)), "another child");
    }

    /// Two children killed by one send form a cohort. The kernel's coalesced
    /// copy, whichever child it names, stands for the lower pid with that
    /// child's siginfo, and the other is notified by its own timer, so the
    /// parent sees the same notifications in the same order either way. A
    /// child outside any cohort is left to `claim`.
    #[test]
    fn a_cohorts_kernel_copy_stands_for_its_lowest_pid_whichever_it_names() {
        let [first, second, alone] = [20, 21, 30].map(DetPid::from_raw);
        let killed = |pid: DetPid| ChildExitSiginfo {
            code: libc::CLD_KILLED,
            pid: pid.as_raw(),
            uid: 1000,
            status: libc::SIGKILL,
        };
        let outcome = |named: DetPid| {
            let mut ledger = ChildExitSigchldLedger::default();
            for child in [first, second, alone] {
                ledger.exit_status.insert(child, killed(child));
                assert!(ledger.own(PARENT, child));
            }
            ledger.join_cohort(first, 7);
            ledger.join_cohort(second, 7);
            assert_eq!(ledger.claim_for_cohort(PARENT, alone), None);
            let kernel = ledger.claim_for_cohort(PARENT, named);
            let timers = [first, second].map(|child| ledger.timer(PARENT, child));
            (kernel, timers, ledger.take_own(PARENT))
        };
        let expected = (
            Some(killed(first)),
            [Some(TimerSend::Nothing), Some(TimerSend::Send)],
            OwnCopy::Deliver(killed(second)),
        );
        assert_eq!(outcome(first), expected);
        assert_eq!(outcome(second), expected);
    }

    /// The scheduler's copy delivered first stands for the exit, with
    /// Linux's siginfo, and the kernel's copy that follows is dropped.
    #[test]
    fn the_schedulers_copy_first_drops_the_kernels() {
        let ledger = &mut owned_child();
        assert_eq!(ledger.timer(PARENT, CHILD), Some(TimerSend::Send));
        assert_eq!(ledger.take_own(PARENT), OwnCopy::Deliver(exited(20, 3)));
        assert!(ledger.claim(PARENT, CHILD), "the kernel's copy is dropped");
        assert!(!ledger.claim(PARENT, CHILD), "dropped once");
        assert_eq!(ledger.take_own(PARENT), OwnCopy::Unowned);
        assert!(ledger.owned.is_empty() && ledger.sent.is_empty());
    }

    /// The kernel's copy delivered while the scheduler's is in flight stands
    /// for the exit, and the scheduler's that follows is dropped.
    #[test]
    fn the_kernels_copy_first_drops_the_schedulers() {
        let ledger = &mut owned_child();
        assert_eq!(ledger.timer(PARENT, CHILD), Some(TimerSend::Send));
        assert!(
            !ledger.claim(PARENT, CHILD),
            "the kernel's copy is delivered"
        );
        assert_eq!(ledger.take_own(PARENT), OwnCopy::Duplicate);
        assert!(ledger.owned.is_empty() && ledger.sent.is_empty());
    }

    /// The kernel's copy delivered before the timer stands for the exit, and
    /// the timer sends nothing.
    #[test]
    fn the_kernels_copy_before_the_timer_means_no_send() {
        let ledger = &mut owned_child();
        assert!(!ledger.claim(PARENT, CHILD));
        assert_eq!(ledger.timer(PARENT, CHILD), Some(TimerSend::Nothing));
        assert_eq!(ledger.timer(PARENT, CHILD), None);
        assert!(ledger.owned.is_empty());
    }

    /// A deferred delivery the timer commits stands for the exit: a later
    /// kernel copy of the child's is dropped.
    #[test]
    fn a_committed_deferred_delivery_stands_for_the_exit() {
        let ledger = &mut owned_child();
        ledger.stand_in(PARENT, CHILD);
        assert!(ledger.claim(PARENT, CHILD));
    }

    /// A scheduler copy is matched to the oldest sent child whose exit no
    /// `SIGCHLD` was delivered for, ahead of one whose kernel copy went first
    /// (that child's copy may have coalesced into the kernel's).
    #[test]
    fn a_schedulers_copy_stands_for_an_undelivered_exit_first() {
        let first = DetPid::from_raw(20);
        let second = DetPid::from_raw(21);
        let mut ledger = ChildExitSigchldLedger::default();
        for child in [first, second] {
            ledger.exit_status.insert(child, exited(child.as_raw(), 1));
            assert!(ledger.own(PARENT, child));
            assert_eq!(ledger.timer(PARENT, child), Some(TimerSend::Send));
        }
        assert!(
            !ledger.claim(PARENT, first),
            "the first child's kernel copy"
        );
        assert_eq!(ledger.take_own(PARENT), OwnCopy::Deliver(exited(21, 1)));
        assert_eq!(ledger.take_own(PARENT), OwnCopy::Duplicate);
        assert_eq!(ledger.take_own(PARENT), OwnCopy::Unowned);
    }

    /// A `SIGCHLD` taken from the shared queue is told by its code and sender:
    /// the kernel's child-exit copy names its child, the tracer's own
    /// `SI_USER` copy is the scheduler's, and any other (a guest's `kill`, a
    /// queued signal, a job-control notice) is another, and a `SI_TKILL` copy
    /// is thread-directed.
    #[test]
    fn a_sigchlds_kind_is_told_by_its_code_and_sender() {
        let siginfo = |code: i32, pid: i32| {
            let mut bytes = [0u8; 128];
            bytes[0..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
            bytes[8..12].copy_from_slice(&code.to_ne_bytes());
            bytes[16..20].copy_from_slice(&pid.to_ne_bytes());
            bytes
        };
        let tracer = 1;
        let dequeue = |copy, admits| Some(ChildExitSigchldControl::Dequeue { copy, admits });
        assert_eq!(
            control_for(&siginfo(libc::CLD_KILLED, 20), tracer, true),
            dequeue(DequeuedCopy::Kernel { child: CHILD }, true)
        );
        assert_eq!(
            control_for(&siginfo(libc::SI_USER, 1), tracer, false),
            dequeue(DequeuedCopy::Scheduler, false)
        );
        for code in [libc::SI_USER, libc::SI_QUEUE] {
            assert_eq!(
                control_for(&siginfo(code, 30), tracer, true),
                dequeue(DequeuedCopy::Other, true)
            );
        }
        assert_eq!(
            control_for(&siginfo(libc::CLD_STOPPED, 20), tracer, true),
            dequeue(DequeuedCopy::Other, true)
        );
        assert_eq!(
            control_for(&siginfo(libc::SI_TKILL, 1), tracer, true),
            dequeue(DequeuedCopy::Thread, true)
        );
    }

    /// At a receiver in a nested PID namespace the scheduler's copy arrives
    /// from pid 0, as Linux reports any sender outside it: the marked
    /// `SI_QUEUE` copy is the scheduler's, and an ancestor's plain `SI_USER`,
    /// an unmarked `SI_QUEUE`, or a marked one with a pid is not
    /// (<https://github.com/rrnewton/hermit/issues/4155>).
    #[test]
    fn a_nested_receivers_own_copy_is_told_by_its_mark() {
        let tracer = 1;
        let mut marked = marked_own_copy(tracer, 0);
        // What the receiver reads: Linux zeroed si_pid.
        marked[16..20].copy_from_slice(&0i32.to_ne_bytes());
        assert_eq!(
            dequeued_copy(&marked, tracer),
            Some(DequeuedCopy::Scheduler)
        );
        let mut unmarked = marked;
        unmarked[24..28].copy_from_slice(&7i32.to_ne_bytes());
        assert_eq!(dequeued_copy(&unmarked, tracer), Some(DequeuedCopy::Other));
        let mut with_pid = marked;
        with_pid[16..20].copy_from_slice(&30i32.to_ne_bytes());
        assert_eq!(dequeued_copy(&with_pid, tracer), Some(DequeuedCopy::Other));
        let mut ancestor = [0u8; 128];
        ancestor[0..4].copy_from_slice(&libc::SIGCHLD.to_ne_bytes());
        ancestor[8..12].copy_from_slice(&libc::SI_USER.to_ne_bytes());
        assert_eq!(dequeued_copy(&ancestor, tracer), Some(DequeuedCopy::Other));
    }

    #[test]
    fn only_exit_codes_are_child_exits() {
        for code in [libc::CLD_EXITED, libc::CLD_KILLED, libc::CLD_DUMPED] {
            assert!(is_child_exit_code(code));
        }
        for code in [
            libc::CLD_TRAPPED,
            libc::CLD_STOPPED,
            libc::CLD_CONTINUED,
            libc::SI_TKILL,
            libc::SI_USER,
            libc::SI_QUEUE,
        ] {
            assert!(!is_child_exit_code(code));
        }
    }
}
