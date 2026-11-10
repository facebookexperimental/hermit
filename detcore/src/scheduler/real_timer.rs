/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Process-owned ITIMER_REAL state. Expiration publishes a pending signal;
//! only an actual shared SIGALRM dequeue can restart an inactive interval.
//!
//! The dequeue the backend acknowledges ([`RealTimers::dequeue`]) only
//! records that the copy left the shared queue. The restart itself waits for
//! the next reconcile point ([`RealTimers::reconcile`]): the `getitimer` or
//! `setitimer` of the process (O1), the end of the signalfd read or
//! `rt_sigtimedwait` that consumed the copy (O2), or the next quiesced
//! boundary (O3). Those are the points at which the traced path's coalescing
//! model (`itimer_coalesce.rs`) restarts its timer, so both models restart a
//! timer at the same committed instant, after the scheduler turn in which the
//! copy was taken has been charged
//! (<https://github.com/rrnewton/hermit/issues/4929>).

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use reverie::CallbackSignalSite;
use reverie::PendingDomain;
use reverie::ProcessSignalPublicationResult;
use reverie::SignalDequeue;
use reverie::SignalProcessId;
use reverie::SignalTaskIdentity;
use serde::Deserialize;
use serde::Serialize;

use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessLife {
    pub detpid: DetPid,
    pub birth: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExpiryId {
    pub life: ProcessLife,
    pub arm: u64,
    pub ordinal: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ItimerSnapshot {
    pub remaining: LogicalTime,
    pub interval: LogicalTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimerFailure {
    Identity,
    Sequence,
    Overflow,
    InvalidPhase,
    Unsupported,
    Publication(ProcessSignalPublicationResult),
    /// Publication may have happened; no receipt was received before cancellation.
    PublicationUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RealTimerPhase {
    Disarmed,
    Active {
        deadline: LogicalTime,
    },
    Publishing {
        deadline: LogicalTime,
        expiry: ExpiryId,
    },
    Inactive {
        last_deadline: LogicalTime,
    },
    /// The copy published for the expiry at `last_deadline` has left the
    /// process's shared queue at an acknowledged dequeue. The timer restarts
    /// on its grid at the next reconcile point ([`RealTimers::reconcile`]).
    Dequeued {
        last_deadline: LogicalTime,
    },
    TerminalFailure {
        expiry: Option<ExpiryId>,
        failure: TimerFailure,
    },
}

#[derive(Clone, Copy, Debug)]
struct RealTimer {
    arm: u64,
    interval: LogicalTime,
    phase: RealTimerPhase,
    last_publication: Option<(ExpiryId, ProcessSignalPublicationResult)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DequeueAck {
    Applied { sequence: u64 },
    Retired { sequence: u64 },
}

#[derive(Clone, Debug)]
struct ProcessTimer {
    life: ProcessLife,
    backend: SignalProcessId,
    tasks: BTreeMap<DetTid, (MmId, SignalTaskIdentity)>,
    retired: bool,
    retired_tasks: BTreeMap<DetTid, (MmId, SignalTaskIdentity)>,
    timer: RealTimer,
    sequence: u64,
    last_dequeue: Option<(SignalDequeue, DequeueAck)>,
}

#[derive(Clone, Debug, Default)]
pub struct RealTimers {
    next_birth: u64,
    next_expiry: u64,
    processes: BTreeMap<DetPid, ProcessTimer>,
    retired: Vec<ProcessTimer>,
}

pub fn active_remaining(deadline: LogicalTime, now: LogicalTime) -> LogicalTime {
    if deadline > now {
        LogicalTime::from_nanos(deadline.as_nanos() - now.as_nanos())
    } else {
        // Linux itimer_get_remtime distinguishes an active overdue timer from
        // an inactive timer. This query minimum never changes its deadline.
        LogicalTime::from_nanos(1_000)
    }
}

fn next_nonce(value: u64) -> Result<u64, TimerFailure> {
    value.checked_add(1).ok_or(TimerFailure::Overflow)
}

fn finite_add(a: LogicalTime, b: LogicalTime) -> Result<LogicalTime, TimerFailure> {
    a.as_nanos()
        .checked_add(b.as_nanos())
        .filter(|n| *n != u64::MAX)
        .map(LogicalTime::from_nanos)
        .ok_or(TimerFailure::Overflow)
}

impl RealTimer {
    fn snapshot(&self, now: LogicalTime) -> Result<ItimerSnapshot, TimerFailure> {
        let remaining = match self.phase {
            // A `Dequeued` timer is reconciled before every query
            // (`Scheduler::reconcile_alarm`), so this arm only answers for it
            // as for the inactive timer it still is until then.
            RealTimerPhase::Disarmed
            | RealTimerPhase::Inactive { .. }
            | RealTimerPhase::Dequeued { .. } => LogicalTime::ZERO,
            RealTimerPhase::Active { deadline } | RealTimerPhase::Publishing { deadline, .. } => {
                active_remaining(deadline, now)
            }
            RealTimerPhase::TerminalFailure { failure, .. } => return Err(failure),
        };
        Ok(ItimerSnapshot {
            remaining,
            interval: self.interval,
        })
    }
}

impl RealTimers {
    /// Called before StartNewThread publishes its first request. A sibling joins
    /// the process sequence; neither a signal nor a timer operation can bind it.
    pub fn bind(
        &mut self,
        detpid: DetPid,
        dettid: DetTid,
        mm: MmId,
        identity: SignalTaskIdentity,
    ) -> Result<ProcessLife, TimerFailure> {
        if identity.tid.as_raw() != dettid.as_raw()
            || identity.process.tgid.as_raw() != detpid.as_raw()
        {
            return Err(TimerFailure::Identity);
        }
        if let Some(old) = self.processes.get(&detpid) {
            if old.backend != identity.process {
                if !old.retired {
                    return Err(TimerFailure::Identity);
                }
                // A numeric PID can be reused, but its old effects remain tied
                // to the old backend generation, never to the replacement.
                let old = self.processes.remove(&detpid).expect("checked process");
                self.retired.push(old);
            } else if old.retired {
                return Err(TimerFailure::Identity);
            }
        }
        if !self.processes.contains_key(&detpid) {
            let birth = next_nonce(self.next_birth)?;
            self.next_birth = birth;
            self.processes.insert(
                detpid,
                ProcessTimer {
                    life: ProcessLife { detpid, birth },
                    backend: identity.process,
                    tasks: BTreeMap::new(),
                    retired: false,
                    retired_tasks: BTreeMap::new(),
                    timer: RealTimer {
                        arm: 0,
                        interval: LogicalTime::ZERO,
                        phase: RealTimerPhase::Disarmed,
                        last_publication: None,
                    },
                    sequence: 0,
                    last_dequeue: None,
                },
            );
        }
        let process = self.processes.get_mut(&detpid).expect("bound process");
        if let Some((old_mm, old_task)) = process.tasks.get(&dettid)
            && (old_task.process != identity.process || (old_mm == &mm && old_task != &identity))
        {
            return Err(TimerFailure::Identity);
        }
        // Successful exec keeps process lifetime and accepted sequence.
        // The scheduler's reconnect gate authorizes the new MmId before
        // this lifecycle binding can replace the task/callback identity.
        process.tasks.insert(dettid, (mm, identity));
        Ok(process.life)
    }

    pub fn validate_task(
        &self,
        detpid: DetPid,
        dettid: DetTid,
        mm: MmId,
        identity: SignalTaskIdentity,
    ) -> Result<ProcessLife, TimerFailure> {
        let p = self.processes.get(&detpid).ok_or(TimerFailure::Identity)?;
        if p.retired
            || p.backend != identity.process
            || p.tasks.get(&dettid) != Some(&(mm, identity))
        {
            return Err(TimerFailure::Identity);
        }
        Ok(p.life)
    }

    pub fn process_identity(&self, pid: DetPid) -> Result<SignalProcessId, TimerFailure> {
        let p = self.processes.get(&pid).ok_or(TimerFailure::Identity)?;
        if p.retired {
            return Err(TimerFailure::Identity);
        }
        Ok(p.backend)
    }

    /// Authenticate cleanup after the last task's consuming hook. A reused PID
    /// names only its current generation; archived lifetimes cannot authenticate
    /// a new retirement notification.
    pub fn retained_process_identity(&self, pid: DetPid) -> Result<SignalProcessId, TimerFailure> {
        self.processes
            .get(&pid)
            .map(|process| process.backend)
            .ok_or(TimerFailure::Identity)
    }

    pub fn task_identity(&self, pid: DetPid, tid: DetTid) -> Option<(MmId, SignalTaskIdentity)> {
        self.processes
            .get(&pid)
            .filter(|p| !p.retired)?
            .tasks
            .get(&tid)
            .copied()
    }

    pub fn live_processes(&self) -> Vec<DetPid> {
        self.processes
            .iter()
            .filter_map(|(pid, p)| (!p.retired).then_some(*pid))
            .collect()
    }

    /// The live processes whose timer waits for its next reconcile point
    /// to restart (`Dequeued`), in pid order.
    pub fn dequeued_processes(&self) -> Vec<DetPid> {
        self.processes
            .iter()
            .filter_map(|(pid, p)| {
                (!p.retired && matches!(p.timer.phase, RealTimerPhase::Dequeued { .. }))
                    .then_some(*pid)
            })
            .collect()
    }

    /// The interval of the live process's current arm, when it is periodic:
    /// the `it_interval` its last `setitimer` installed, which expiries and
    /// restarts keep. `None` for a one-shot or disarmed timer.
    pub fn interval(&self, detpid: DetPid) -> Option<LogicalTime> {
        self.processes
            .get(&detpid)
            .filter(|p| !p.retired)
            .map(|p| p.timer.interval)
            .filter(|interval| *interval != LogicalTime::ZERO)
    }

    /// A successful in-process exec keeps its timer and accepted sequence but
    /// replaces the callback's address-space/task identity. The lifecycle RPC
    /// authenticates the saved PrepareExec transition before calling this.
    pub fn rebind_after_exec(
        &mut self,
        detpid: DetPid,
        dettid: DetTid,
        previous_mm: MmId,
        current_mm: MmId,
        identity: SignalTaskIdentity,
    ) -> Result<(), TimerFailure> {
        let process = self
            .processes
            .get_mut(&detpid)
            .ok_or(TimerFailure::Identity)?;
        if process.retired
            || process.backend != identity.process
            || identity.tid.as_raw() != dettid.as_raw()
            || identity.process.tgid.as_raw() != detpid.as_raw()
            || current_mm != previous_mm.for_exec(detpid)
            || !process
                .tasks
                .get(&dettid)
                .is_some_and(|(mm, _)| *mm == previous_mm)
        {
            return Err(TimerFailure::Identity);
        }
        process.tasks.insert(dettid, (current_mm, identity));
        Ok(())
    }

    pub fn validate_effect(
        &self,
        pid: DetPid,
        tid: DetTid,
        mm: MmId,
        identity: SignalTaskIdentity,
        process: SignalProcessId,
    ) -> Result<bool, TimerFailure> {
        if identity.process != process {
            return Err(TimerFailure::Identity);
        }
        let p = self
            .processes
            .get(&pid)
            .filter(|p| p.backend == process)
            .or_else(|| {
                self.retired
                    .iter()
                    .find(|p| p.life.detpid == pid && p.backend == process)
            })
            .ok_or(TimerFailure::Identity)?;
        if p.tasks.get(&tid) != Some(&(mm, identity))
            && p.retired_tasks.get(&tid) != Some(&(mm, identity))
        {
            return Err(TimerFailure::Identity);
        }
        Ok(!p.retired && p.tasks.get(&tid) == Some(&(mm, identity)))
    }

    pub fn validate_site(
        &self,
        detpid: DetPid,
        dettid: DetTid,
        mm: MmId,
        site: CallbackSignalSite,
    ) -> Result<ProcessLife, TimerFailure> {
        self.validate_task(
            detpid,
            dettid,
            mm,
            SignalTaskIdentity {
                process: site.process,
                tid: site.tid,
                task_generation: site.task_generation,
            },
        )
    }

    pub fn snapshot(
        &self,
        detpid: DetPid,
        now: LogicalTime,
    ) -> Result<ItimerSnapshot, TimerFailure> {
        let p = self.processes.get(&detpid).ok_or(TimerFailure::Identity)?;
        if p.retired {
            return Err(TimerFailure::Identity);
        }
        p.timer.snapshot(now)
    }

    /// Compute the complete replacement before changing any current state.
    /// The caller replaces its heap entry in the same scheduler critical section.
    pub fn replace(
        &mut self,
        detpid: DetPid,
        dettid: DetTid,
        now: LogicalTime,
        value: LogicalTime,
        interval: LogicalTime,
    ) -> Result<(ItimerSnapshot, Option<LogicalTime>), TimerFailure> {
        let p = self
            .processes
            .get_mut(&detpid)
            .ok_or(TimerFailure::Identity)?;
        if p.retired || !p.tasks.contains_key(&dettid) {
            return Err(TimerFailure::Identity);
        }
        let old = p.timer.snapshot(now)?;
        let arm = next_nonce(p.timer.arm)?;
        let deadline = if value == LogicalTime::ZERO {
            None
        } else {
            Some(finite_add(now, value)?)
        };
        if interval.is_indefinite() {
            return Err(TimerFailure::Overflow);
        }
        p.timer.arm = arm;
        p.timer.interval = if deadline.is_some() {
            interval
        } else {
            LogicalTime::ZERO
        };
        p.timer.phase = match deadline {
            Some(deadline) => RealTimerPhase::Active { deadline },
            None => RealTimerPhase::Disarmed,
        };
        Ok((old, deadline))
    }

    pub fn expire(
        &mut self,
        detpid: DetPid,
        deadline: LogicalTime,
    ) -> Result<ExpiryId, TimerFailure> {
        let p = self
            .processes
            .get_mut(&detpid)
            .ok_or(TimerFailure::Identity)?;
        if p.retired || p.timer.phase != (RealTimerPhase::Active { deadline }) {
            return Err(TimerFailure::InvalidPhase);
        }
        let ordinal = next_nonce(self.next_expiry)?;
        self.next_expiry = ordinal;
        let expiry = ExpiryId {
            life: p.life,
            arm: p.timer.arm,
            ordinal,
        };
        p.timer.phase = RealTimerPhase::Publishing { deadline, expiry };
        Ok(expiry)
    }

    /// The expiry at `deadline` of `detpid`'s armed timer, which Linux
    /// discards as it is generated: `it_real_fn` sends SIGALRM to the leader,
    /// whose `prepare_signal` drops it (`sig_ignored`), and returns
    /// `HRTIMER_NORESTART`, so the timer stays stopped until a shared-queue
    /// SIGALRM dequeue restarts it (`posixtimer_rearm_itimer`)
    /// (<https://github.com/rrnewton/hermit/issues/4929>). Nothing is
    /// published: `Active { deadline }` becomes `Inactive { last_deadline:
    /// deadline }` directly, with no publication recorded, and the next
    /// acknowledged dequeue restarts it on its grid ([`Self::dequeue`],
    /// [`Self::reconcile`]). Any other phase is refused, as for
    /// [`Self::expire`].
    pub fn discard_expiry(
        &mut self,
        detpid: DetPid,
        deadline: LogicalTime,
    ) -> Result<(), TimerFailure> {
        let p = self
            .processes
            .get_mut(&detpid)
            .ok_or(TimerFailure::Identity)?;
        if p.retired || p.timer.phase != (RealTimerPhase::Active { deadline }) {
            return Err(TimerFailure::InvalidPhase);
        }
        p.timer.phase = RealTimerPhase::Inactive {
            last_deadline: deadline,
        };
        Ok(())
    }

    pub fn publication(
        &mut self,
        expiry: ExpiryId,
        outcome: ProcessSignalPublicationResult,
    ) -> Result<(), TimerFailure> {
        let p = self
            .processes
            .get_mut(&expiry.life.detpid)
            .ok_or(TimerFailure::Identity)?;
        if p.life != expiry.life || p.timer.arm != expiry.arm {
            return Err(TimerFailure::Identity);
        }
        if p.timer.last_publication == Some((expiry, outcome)) {
            return Ok(());
        }
        let RealTimerPhase::Publishing {
            deadline,
            expiry: expected,
        } = p.timer.phase
        else {
            return Err(TimerFailure::InvalidPhase);
        };
        if expected != expiry {
            return Err(TimerFailure::Identity);
        }
        p.timer.last_publication = Some((expiry, outcome));
        match outcome {
            ProcessSignalPublicationResult::Committed(_) => {
                p.timer.phase = RealTimerPhase::Inactive {
                    last_deadline: deadline,
                };
                Ok(())
            }
            ProcessSignalPublicationResult::FailedAfterCommit { .. } => {
                // The retained receipt proves publication. Terminal state is
                // distinct from a pre-publication rejection or an unknown result.
                p.timer.phase = RealTimerPhase::Inactive {
                    last_deadline: deadline,
                };
                self.fail(expiry.life.detpid, TimerFailure::Publication(outcome));
                Err(TimerFailure::Publication(outcome))
            }
            ProcessSignalPublicationResult::RejectedBeforeCommit(_) => {
                p.timer.phase = RealTimerPhase::TerminalFailure {
                    expiry: Some(expiry),
                    failure: TimerFailure::Publication(outcome),
                };
                Err(TimerFailure::Publication(outcome))
            }
        }
    }

    pub fn fail(&mut self, detpid: DetPid, failure: TimerFailure) {
        if let Some(p) = self.processes.get_mut(&detpid) {
            if matches!(p.timer.phase, RealTimerPhase::TerminalFailure { .. }) {
                return;
            }
            let expiry = match p.timer.phase {
                RealTimerPhase::Publishing { expiry, .. } => Some(expiry),
                _ => p.timer.last_publication.map(|(expiry, _)| expiry),
            };
            p.timer.phase = RealTimerPhase::TerminalFailure { expiry, failure };
        }
    }

    pub fn retire_task(&mut self, detpid: DetPid, dettid: DetTid) {
        if let Some(p) = self.processes.get_mut(&detpid)
            && let Some(identity) = p.tasks.remove(&dettid)
        {
            p.retired_tasks.insert(dettid, identity);
        }
    }

    pub fn retire_process(&mut self, detpid: DetPid) {
        if let Some(p) = self.processes.get_mut(&detpid) {
            p.retired = true;
            if matches!(p.timer.phase, RealTimerPhase::Publishing { .. }) {
                self.fail(detpid, TimerFailure::PublicationUnknown);
            } else if !matches!(p.timer.phase, RealTimerPhase::TerminalFailure { .. }) {
                p.timer.phase = RealTimerPhase::Disarmed;
                p.timer.interval = LogicalTime::ZERO;
            }
        }
    }

    /// Acknowledgment and the current-arm transition are one atomic scheduler
    /// operation. Event provenance is deliberately irrelevant to rearming.
    ///
    /// The transition only records the dequeue: an `Inactive` periodic timer
    /// of a live process becomes `Dequeued`, and [`Self::reconcile`] restarts
    /// it. Any other phase is left as it is: an armed or disarmed timer is not
    /// restarted by a dequeue, a one-shot timer has nothing to restart, and a
    /// timer already `Dequeued` restarts once.
    pub fn dequeue(
        &mut self,
        detpid: DetPid,
        event: SignalDequeue,
    ) -> Result<DequeueAck, TimerFailure> {
        let p = if self
            .processes
            .get(&detpid)
            .is_some_and(|p| p.backend == event.process)
        {
            self.processes.get_mut(&detpid).expect("checked process")
        } else {
            self.retired
                .iter_mut()
                .find(|p| p.life.detpid == detpid && p.backend == event.process)
                .ok_or(TimerFailure::Identity)?
        };
        if let Some((previous, ack)) = p.last_dequeue
            && previous == event
        {
            return Ok(ack);
        }
        if next_nonce(p.sequence)? != event.sequence {
            return Err(TimerFailure::Sequence);
        }
        if !p.retired
            && event.domain == PendingDomain::Process
            && event.event.signal() == libc::SIGALRM
            && p.timer.interval != LogicalTime::ZERO
            && let RealTimerPhase::Inactive { last_deadline } = p.timer.phase
        {
            p.timer.phase = RealTimerPhase::Dequeued { last_deadline };
        }
        let ack = if p.retired {
            DequeueAck::Retired {
                sequence: event.sequence,
            }
        } else {
            DequeueAck::Applied {
                sequence: event.sequence,
            }
        };
        p.sequence = event.sequence;
        p.last_dequeue = Some((event, ack));
        Ok(ack)
    }

    /// Restart `detpid`'s timer at reconcile point `t` if its last copy was
    /// dequeued (`Dequeued`): at the first point of its grid
    /// (`last_deadline + k * interval`, `k >= 1`) after `t`, as Linux's
    /// `hrtimer_forward` places it, so missed periods are dropped. The traced
    /// model restarts on the same grid (`timed_waiters::checked_grid_after`).
    /// Returns the new deadline; `None` in every other phase, and for a
    /// retired process. A deadline that would not fit, or a `t` before the
    /// expiry whose copy was dequeued, leaves the timer as it was and fails.
    pub fn reconcile(
        &mut self,
        detpid: DetPid,
        t: LogicalTime,
    ) -> Result<Option<LogicalTime>, TimerFailure> {
        let Some(p) = self.processes.get_mut(&detpid).filter(|p| !p.retired) else {
            return Ok(None);
        };
        let RealTimerPhase::Dequeued { last_deadline } = p.timer.phase else {
            return Ok(None);
        };
        // Only a periodic arm becomes `Dequeued`, and replacing the arm
        // leaves that phase.
        let period =
            NonZeroU64::new(p.timer.interval.as_nanos()).ok_or(TimerFailure::InvalidPhase)?;
        // The copy was made by a pop at or after `last_deadline`, and every
        // reconcile point comes later, at a committed time or a caller's
        // global time, neither of which moves back. An earlier `t` is a
        // broken invariant, not a time to round up.
        if t < last_deadline {
            return Err(TimerFailure::InvalidPhase);
        }
        let deadline = super::timed_waiters::checked_grid_after(last_deadline, period, t)
            .filter(|deadline| !deadline.is_indefinite())
            .ok_or(TimerFailure::Overflow)?;
        p.timer.phase = RealTimerPhase::Active { deadline };
        Ok(Some(deadline))
    }
}

#[cfg(test)]
mod tests {
    use reverie::Pid;
    use reverie::ProcessAlarmSignalDisposition;
    use reverie::ProcessSignalPublication;
    use reverie::SignalConsumer;
    use reverie::SignalEvent;
    use reverie::SignalTarget;

    use super::*;

    fn time(n: u64) -> LogicalTime {
        LogicalTime::from_nanos(n)
    }
    fn identity(generation: u64) -> SignalTaskIdentity {
        SignalTaskIdentity {
            process: SignalProcessId {
                tgid: Pid::from_raw(100),
                generation,
            },
            tid: Pid::from_raw(100),
            task_generation: generation,
        }
    }
    fn bound() -> (RealTimers, DetPid, MmId) {
        let pid = DetPid::from_raw(100);
        let mm = MmId::initial(pid);
        let mut timers = RealTimers::default();
        timers.bind(pid, pid, mm, identity(1)).unwrap();
        (timers, pid, mm)
    }
    fn effect(generation: u64, sequence: u64) -> SignalDequeue {
        let process = identity(generation).process;
        let mut info = [0; 128];
        info[..4].copy_from_slice(&libc::SIGALRM.to_ne_bytes());
        // Software provenance is deliberate: the current timer, not its
        // producer's generation, decides whether a shared dequeue rearms.
        SignalDequeue {
            process,
            sequence,
            consumer: SignalConsumer::SignalFd,
            domain: PendingDomain::Process,
            event: SignalEvent::new(
                libc::SIGALRM,
                info,
                SignalTarget::Process { pid: process.tgid },
            )
            .unwrap(),
        }
    }
    fn publish(timers: &mut RealTimers, pid: DetPid, deadline: u64) {
        let expiry = timers.expire(pid, time(deadline)).unwrap();
        timers
            .publication(
                expiry,
                ProcessSignalPublicationResult::Committed(ProcessSignalPublication {
                    process: identity(1).process,
                    disposition: ProcessAlarmSignalDisposition::Caught,
                    pending_generation: 123,
                    coalesced: true,
                }),
            )
            .unwrap();
    }

    #[test]
    fn old_pending_alarm_rearms_only_the_current_inactive_arm() {
        for replacement_expired in [false, true] {
            let (mut timers, pid, _) = bound();
            timers
                .replace(pid, pid, time(0), time(10), time(10))
                .unwrap();
            publish(&mut timers, pid, 10);
            let deadline = if replacement_expired { 30 } else { 50 };
            timers
                .replace(pid, pid, time(20), time(deadline - 20), time(10))
                .unwrap();
            if replacement_expired {
                publish(&mut timers, pid, deadline);
            }
            timers.dequeue(pid, effect(1, 1)).unwrap();
            let next = timers.reconcile(pid, time(35)).unwrap();
            assert_eq!(next, replacement_expired.then_some(time(40)));
            assert_eq!(
                timers.snapshot(pid, time(35)).unwrap().remaining,
                time(if replacement_expired { 5 } else { 15 })
            );
        }
    }

    #[test]
    fn disarm_clears_interval_and_dequeue_does_not_restart_it() {
        let (mut timers, pid, _) = bound();
        timers
            .replace(pid, pid, time(0), time(10), time(10))
            .unwrap();
        publish(&mut timers, pid, 10);
        timers
            .replace(pid, pid, time(20), time(0), time(999))
            .unwrap();
        assert_eq!(
            timers.snapshot(pid, time(35)).unwrap(),
            ItimerSnapshot::default()
        );
        timers.dequeue(pid, effect(1, 1)).unwrap();
        assert_eq!(timers.reconcile(pid, time(35)).unwrap(), None);
    }

    #[test]
    fn startup_sequence_rejects_gaps_conflicts_and_reused_processes() {
        let (mut timers, pid, mm) = bound();
        assert_eq!(
            timers.dequeue(pid, effect(1, 2)),
            Err(TimerFailure::Sequence)
        );
        let original = effect(1, 1);
        let ack = timers.dequeue(pid, original).unwrap();
        assert_eq!(timers.dequeue(pid, original).unwrap(), ack);
        let mut conflict = original;
        conflict.consumer = SignalConsumer::SignalTimedWait;
        assert_eq!(timers.dequeue(pid, conflict), Err(TimerFailure::Sequence));
        assert_eq!(
            timers.bind(pid, pid, mm, identity(2)),
            Err(TimerFailure::Identity)
        );
        timers.retire_task(pid, pid);
        timers.retire_process(pid);
        let replacement = timers.bind(pid, pid, mm, identity(2)).unwrap();
        assert_eq!(replacement.birth, 2);
        assert_eq!(
            timers.validate_effect(pid, pid, mm, identity(1), identity(1).process),
            Ok(false)
        );
        assert_eq!(
            timers.dequeue(pid, effect(1, 2)).unwrap(),
            DequeueAck::Retired { sequence: 2 }
        );
        assert_eq!(
            timers.snapshot(pid, time(35)).unwrap(),
            ItimerSnapshot::default()
        );
        assert_eq!(
            timers.dequeue(pid, effect(2, 1)).unwrap(),
            DequeueAck::Applied { sequence: 1 }
        );
    }

    #[test]
    fn overdue_query_is_one_microsecond_without_changing_deadline() {
        let (mut timers, pid, _) = bound();
        timers
            .replace(pid, pid, time(0), time(10), time(7))
            .unwrap();
        assert_eq!(
            timers.snapshot(pid, time(35)).unwrap(),
            ItimerSnapshot {
                remaining: time(1000),
                interval: time(7)
            }
        );
        let expiry = timers.expire(pid, time(10)).unwrap();
        assert_eq!(
            timers.snapshot(pid, time(35)).unwrap().remaining,
            time(1000)
        );
        timers.retire_process(pid);
        assert_eq!(
            timers.processes[&pid].timer.phase,
            RealTimerPhase::TerminalFailure {
                expiry: Some(expiry),
                failure: TimerFailure::PublicationUnknown
            }
        );
    }

    #[test]
    fn overflow_never_wraps_a_deadline_or_mutates_a_valid_arm() {
        let (mut timers, pid, _) = bound();
        timers
            .replace(pid, pid, time(0), time(10), time(10))
            .unwrap();
        assert_eq!(
            timers.replace(pid, pid, time(u64::MAX - 2), time(2), time(0)),
            Err(TimerFailure::Overflow)
        );
        assert_eq!(
            timers.snapshot(pid, time(0)).unwrap(),
            ItimerSnapshot {
                remaining: time(10),
                interval: time(10)
            }
        );
        publish(&mut timers, pid, 10);
        timers.dequeue(pid, effect(1, 1)).unwrap();
        assert_eq!(
            timers.reconcile(pid, time(u64::MAX - 1)),
            Err(TimerFailure::Overflow)
        );
        assert_eq!(timers.snapshot(pid, time(0)).unwrap().remaining, time(0));
        // The refused restart left the dequeued arm as it was.
        assert_eq!(timers.dequeued_processes(), vec![pid]);
        assert_eq!(timers.reconcile(pid, time(35)).unwrap(), Some(time(40)));
    }

    /// An acknowledged dequeue only records that the copy left the shared
    /// queue. The timer restarts at the next reconcile point, at the first
    /// point of its grid strictly after that point, so the periods missed
    /// before it are dropped (a reconcile point on the grid restarts at the
    /// next one). Before the dequeue a reconcile point restarts nothing; a
    /// second dequeue before the reconcile point restarts it once; and once
    /// restarted it does not restart again without a new expiry and dequeue.
    #[test]
    fn a_dequeue_restarts_the_timer_at_the_next_reconcile_point() {
        for (reconcile_at, restart) in [(12, 20), (20, 30), (25, 30), (39, 40)] {
            let (mut timers, pid, _) = bound();
            timers
                .replace(pid, pid, time(0), time(10), time(10))
                .unwrap();
            publish(&mut timers, pid, 10);
            assert_eq!(timers.reconcile(pid, time(reconcile_at)).unwrap(), None);
            assert_eq!(
                timers.snapshot(pid, time(reconcile_at)).unwrap().remaining,
                time(0)
            );
            assert!(timers.dequeued_processes().is_empty());
            timers.dequeue(pid, effect(1, 1)).unwrap();
            assert_eq!(timers.dequeued_processes(), vec![pid]);
            timers.dequeue(pid, effect(1, 2)).unwrap();
            assert_eq!(
                timers.reconcile(pid, time(reconcile_at)).unwrap(),
                Some(time(restart))
            );
            assert!(timers.dequeued_processes().is_empty());
            assert_eq!(
                timers.snapshot(pid, time(reconcile_at)).unwrap(),
                ItimerSnapshot {
                    remaining: time(restart - reconcile_at),
                    interval: time(10)
                }
            );
            assert_eq!(timers.reconcile(pid, time(reconcile_at)).unwrap(), None);
        }
    }

    /// The controlled timer restarts on the traced model's grid
    /// (`timed_waiters::checked_grid_after`, which `grid_after` saturates),
    /// so both models restart a dequeued copy at the same instant for the
    /// same reconcile point. A reconcile point before the dequeued expiry is
    /// a broken invariant: it fails and leaves the timer dequeued.
    #[test]
    fn a_restart_uses_the_traced_grid_and_refuses_an_earlier_reconcile_point() {
        let dequeued = || {
            let (mut timers, pid, _) = bound();
            timers
                .replace(pid, pid, time(0), time(10), time(10))
                .unwrap();
            publish(&mut timers, pid, 10);
            timers.dequeue(pid, effect(1, 1)).unwrap();
            (timers, pid)
        };
        for t in [10, 11, 19, 20, 21, 99, 100] {
            let (mut timers, pid) = dequeued();
            assert_eq!(
                timers.reconcile(pid, time(t)).unwrap(),
                Some(crate::scheduler::timed_waiters::grid_after(
                    time(10),
                    time(10),
                    time(t)
                ))
            );
        }
        let (mut timers, pid) = dequeued();
        assert_eq!(
            timers.reconcile(pid, time(9)),
            Err(TimerFailure::InvalidPhase)
        );
        assert_eq!(timers.dequeued_processes(), vec![pid]);
        assert_eq!(timers.reconcile(pid, time(10)).unwrap(), Some(time(20)));
    }

    /// A dequeue restarts only an inactive periodic timer: a one-shot
    /// timer's dequeued expiry leaves it inactive, an armed timer stays as it
    /// is, a replacement while dequeued wins over the restart, and a retired
    /// process restarts nothing.
    #[test]
    fn a_dequeue_restarts_no_one_shot_armed_replaced_or_retired_timer() {
        let (mut timers, pid, _) = bound();
        timers
            .replace(pid, pid, time(0), time(10), time(0))
            .unwrap();
        publish(&mut timers, pid, 10);
        timers.dequeue(pid, effect(1, 1)).unwrap();
        assert!(timers.dequeued_processes().is_empty());
        assert_eq!(timers.reconcile(pid, time(15)).unwrap(), None);
        assert_eq!(
            timers.snapshot(pid, time(15)).unwrap(),
            ItimerSnapshot::default()
        );

        timers
            .replace(pid, pid, time(20), time(10), time(10))
            .unwrap();
        timers.dequeue(pid, effect(1, 2)).unwrap();
        assert_eq!(timers.reconcile(pid, time(25)).unwrap(), None);
        assert_eq!(timers.snapshot(pid, time(25)).unwrap().remaining, time(5));

        publish(&mut timers, pid, 30);
        timers.dequeue(pid, effect(1, 3)).unwrap();
        timers
            .replace(pid, pid, time(35), time(100), time(100))
            .unwrap();
        assert_eq!(timers.reconcile(pid, time(36)).unwrap(), None);
        assert_eq!(timers.snapshot(pid, time(36)).unwrap().remaining, time(99));

        publish(&mut timers, pid, 135);
        timers.dequeue(pid, effect(1, 4)).unwrap();
        assert_eq!(timers.dequeued_processes(), vec![pid]);
        timers.retire_task(pid, pid);
        timers.retire_process(pid);
        assert!(timers.dequeued_processes().is_empty());
        assert_eq!(timers.reconcile(pid, time(140)).unwrap(), None);
    }

    /// The interval names a live periodic arm, expiries included, and
    /// nothing else.
    #[test]
    fn interval_names_only_a_live_periodic_arm() {
        let (mut timers, pid, _) = bound();
        assert_eq!(timers.interval(pid), None);
        timers
            .replace(pid, pid, time(0), time(10), time(7))
            .unwrap();
        assert_eq!(timers.interval(pid), Some(time(7)));
        publish(&mut timers, pid, 10);
        assert_eq!(timers.interval(pid), Some(time(7)));
        timers
            .replace(pid, pid, time(20), time(10), time(0))
            .unwrap();
        assert_eq!(timers.interval(pid), None);
        timers
            .replace(pid, pid, time(20), time(10), time(7))
            .unwrap();
        timers.retire_task(pid, pid);
        timers.retire_process(pid);
        assert_eq!(timers.interval(pid), None);
    }
}
