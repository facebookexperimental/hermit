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

/// Whether `code` is a child-exit `si_code` (`CLD_EXITED`, `CLD_KILLED`,
/// `CLD_DUMPED`), as opposed to a job-control one or a sender's.
pub fn is_child_exit_code(code: i32) -> bool {
    matches!(code, libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED)
}

/// The control message for a `SIGCHLD` with siginfo `info`: the kernel's
/// copy of a child's exit notification (a child-exit `si_code`), or a copy
/// the scheduler sent (`SI_USER` from `tracer`, the tracer's process, which
/// is never a guest process; tracer and guests share a PID namespace).
/// `None` for any other.
pub fn control_for(info: &[u8; 128], tracer: u32) -> Option<ChildExitSigchldControl> {
    let (code, pid) = siginfo_code_and_pid(info);
    if is_child_exit_code(code) {
        Some(ChildExitSigchldControl::ClaimKernelCopy {
            child: DetPid::from_raw(pid),
        })
    } else if code == libc::SI_USER && pid as u32 == tracer {
        Some(ChildExitSigchldControl::TakeOwnCopy)
    } else {
        None
    }
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

/// The answer to a [`ChildExitSigchldControl`].
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum ChildExitSigchldAnswer {
    Recorded,
    Claimed(bool),
    OwnCopy(OwnCopy),
    /// The kernel's copy is delivered, with this siginfo: it stands for
    /// another member of its child's SIGKILL cohort.
    ClaimedFor(ChildExitSiginfo),
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
                self.fire_alarm(parent, target, nix::sys::signal::Signal::SIGCHLD);
                true
            }
        }
    }

    /// Answer a [`ChildExitSigchldControl`] from `dtid`.
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
            ChildExitSigchldControl::TakeOwnCopy => {
                let parent = self.sigchld_process(dtid);
                let own = self.child_exit_sigchld.take_own(parent);
                if own == OwnCopy::Duplicate {
                    debug!(
                        "[dtid {}] dropping the scheduler's SIGCHLD: the kernel's was delivered first",
                        dtid
                    );
                }
                ChildExitSigchldAnswer::OwnCopy(own)
            }
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

    /// Only the kernel's child-exit copy and the tracer's own `SI_USER` copy
    /// are child-exit `SIGCHLD`s; a guest's own `kill` is not, nor is
    /// anything else.
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
        assert_eq!(
            control_for(&siginfo(libc::CLD_KILLED, 20), tracer),
            Some(ChildExitSigchldControl::ClaimKernelCopy { child: CHILD })
        );
        assert_eq!(
            control_for(&siginfo(libc::SI_USER, 1), tracer),
            Some(ChildExitSigchldControl::TakeOwnCopy)
        );
        assert_eq!(control_for(&siginfo(libc::SI_USER, 30), tracer), None);
        assert_eq!(control_for(&siginfo(libc::SI_TKILL, 1), tracer), None);
        assert_eq!(control_for(&siginfo(libc::CLD_STOPPED, 20), tracer), None);
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
