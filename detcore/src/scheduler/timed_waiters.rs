/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use nix::sys::signal::Signal;

use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// Encapsulate the set of threads that are waiting for a specific time in the future.
///
/// It's possible (but unlikely) that multiple threads are waiting for the same
/// nanosecond, and this structure must break that symmetry.
#[derive(Debug, Clone, Default)]
pub struct TimedEvents {
    // Inner btreeset is *always* non-empty:
    map: BTreeMap<LogicalTime, BTreeSet<TimedEvent>>,

    // Keep one alarm(2)/setitimer(2) event per process and one event per POSIX timer id.
    signal_timers: BTreeMap<SignalTimerId, SignalTimerState>,
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#869)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SignalTimerId {
    Alarm(DetPid),
    Posix(DetPid, i32),
    /// A deterministic child-exit `SIGCHLD`, synthesized at the child's
    /// scheduler-ordered `Exit` grant (`t_exit`) rather than delivered by the
    /// host-async kernel signal. `child` is the exiting process (a unique
    /// coalescing key so multiple reaped children never collide); `parent` is
    /// the process that receives the signal. Unlike `Alarm`/`Posix`, a
    /// `ChildExit` event is one-shot and is never re-armed or cancelled, so it
    /// is inserted directly into the timed `map` and bypasses the
    /// `signal_timers` re-arm bookkeeping (see `insert_child_exit`).
    ChildExit {
        child: DetPid,
        parent: DetPid,
    },
}

impl SignalTimerId {
    /// The process the timed signal is delivered to. For `ChildExit` this is the
    /// *parent* (the reaper), not the exiting child.
    pub(super) fn process(self) -> DetPid {
        match self {
            Self::Alarm(pid) | Self::Posix(pid, _) => pid,
            Self::ChildExit { parent, .. } => parent,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SignalTimerState {
    deadline: LogicalTime,
    interval: LogicalTime,
}

/// An event that occurs at a particular time in the execution, typically at an offset in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimedEvent {
    // An upcoming timer signal, destined for a process with a preferred target thread.
    SignalEvt(SignalTimerId, DetTid, Signal),

    /// A timed event on a particular thread (sleep, timeout, etc)
    ThreadEvt(DetTid),
}

impl fmt::Display for TimedEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TimedEvent::ThreadEvt(dt) => write!(f, "ThreadEvt({})", dt),
            TimedEvent::SignalEvt(id, dt, sig) => {
                write!(f, "SignalEvt({:?},{},{})", id, dt, sig)
            }
        }
    }
}

impl TimedEvents {
    pub fn insert(&mut self, ns: LogicalTime, dt: DetTid) {
        let set = self.map.entry(ns).or_default();
        if !set.insert(TimedEvent::ThreadEvt(dt)) {
            panic!(
                "TimedEvents::insert should not take a DetTid which is *already* in the set: {}",
                dt
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    fn insert_signal_timer(
        &mut self,
        id: SignalTimerId,
        ns: LogicalTime,
        dt: DetTid,
        sig: Signal,
        interval: LogicalTime,
    ) -> Option<SignalTimerState> {
        let old = self.signal_timers.insert(
            id,
            SignalTimerState {
                deadline: ns,
                interval,
            },
        );
        self.clear_old_signal_timer(id, old);

        let set = self.map.entry(ns).or_default();
        let evt = TimedEvent::SignalEvt(id, dt, sig);
        if !set.insert(evt) {
            panic!(
                "TimedEvents::insert_signal_timer should not insert an event which is already in the set: {}",
                evt
            );
        }
        old
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    // Return the last alarm state for this pid, if any.
    pub fn insert_alarm(
        &mut self,
        ns: LogicalTime,
        dp: DetPid,
        dt: DetTid,
        sig: Signal,
        interval: LogicalTime,
    ) -> Option<(LogicalTime, LogicalTime)> {
        self.insert_signal_timer(SignalTimerId::Alarm(dp), ns, dt, sig, interval)
            .map(|state| (state.deadline, state.interval))
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    fn clear_old_signal_timer(&mut self, id: SignalTimerId, old: Option<SignalTimerState>) {
        if let Some(state) = old {
            // The `map` entry may already be gone if the alarm fired (was
            // popped by `pop_if_before`) before being cleared. Clearing an
            // already-fired alarm is a no-op rather than an invariant break.
            let Some(set) = self.map.get_mut(&state.deadline) else {
                return;
            };

            // Could use a drain_filter here, but it is nightly only:
            let mut to_remove = None;
            for evt in set.iter() {
                if matches!(evt, TimedEvent::SignalEvt(evt_id, _, _) if *evt_id == id) {
                    assert!(to_remove.is_none());
                    to_remove = Some(*evt);
                }
            }
            if let Some(evt) = to_remove {
                assert!(set.remove(&evt));
            }

            // Preserve the invariant that `map` never holds an empty set, which
            // `is_empty()` and `iter()` rely on.
            if set.is_empty() {
                self.map.remove(&state.deadline);
            }
        }
    }

    // Return the time of any previous alarm on this process.
    pub fn remove_alarm(&mut self, dp: DetPid) -> Option<(LogicalTime, LogicalTime)> {
        self.remove_signal_timer(SignalTimerId::Alarm(dp))
            .map(|state| (state.deadline, state.interval))
    }

    /// Register a one-shot, deterministic child-exit `SIGCHLD` to be delivered to
    /// `parent` (via thread `parent_tid`) at logical time `ns` (the child's
    /// `Exit` grant time plus a tick). Inserted directly into the timed `map`,
    /// deliberately bypassing the `signal_timers` re-arm/cancel bookkeeping used
    /// by `alarm`/`setitimer`/POSIX timers: a child exit fires exactly once and
    /// is never re-armed or replaced, and its key (`ChildExit{child,parent}`) is
    /// unique per exiting child, so it cannot collide with a concurrent
    /// `Alarm`/`Posix` timer on the same process. If an identical event is
    /// already queued at `ns` (the same child reported twice), the insert is a
    /// no-op — the redundant delivery is coalesced, matching Linux `SIGCHLD`.
    pub fn insert_child_exit(
        &mut self,
        ns: LogicalTime,
        child: DetPid,
        parent: DetPid,
        parent_tid: DetTid,
    ) {
        let evt = TimedEvent::SignalEvt(
            SignalTimerId::ChildExit { child, parent },
            parent_tid,
            Signal::SIGCHLD,
        );
        // BTreeSet::insert returns false on a duplicate; coalescing it is
        // intentional (see doc comment) rather than a panic.
        self.map.entry(ns).or_default().insert(evt);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn insert_posix_timer(
        &mut self,
        ns: LogicalTime,
        dp: DetPid,
        dt: DetTid,
        timer_id: i32,
        sig: Signal,
        interval: LogicalTime,
    ) {
        self.insert_signal_timer(SignalTimerId::Posix(dp, timer_id), ns, dt, sig, interval);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn remove_posix_timer(&mut self, dp: DetPid, timer_id: i32) {
        self.remove_signal_timer(SignalTimerId::Posix(dp, timer_id));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    fn remove_signal_timer(&mut self, id: SignalTimerId) -> Option<SignalTimerState> {
        let old = self.signal_timers.remove(&id);
        self.clear_old_signal_timer(id, old);
        old
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review non-mutating logical alarm lookup.
    pub fn alarm_time(&self, dp: DetPid) -> Option<LogicalTime> {
        self.signal_timers
            .get(&SignalTimerId::Alarm(dp))
            .map(|state| state.deadline)
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn remove_process_timers(&mut self, dp: DetPid) {
        let ids: Vec<_> = self
            .signal_timers
            .keys()
            .copied()
            .filter(|id| match id {
                SignalTimerId::Alarm(pid) | SignalTimerId::Posix(pid, _) => *pid == dp,
                // `ChildExit` events are never stored in `signal_timers`, so this
                // arm is unreachable in practice; it exists only for exhaustiveness.
                SignalTimerId::ChildExit { .. } => false,
            })
            .collect();
        for id in ids {
            self.remove_signal_timer(id);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Return the next event if its target time of occurrence is before the supplied time.
    /// Being a "pop", this destructively removes the entry.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn pop_if_before(
        &mut self,
        current_time: LogicalTime,
    ) -> Option<(LogicalTime, TimedEvent)> {
        let (time_ns, evt) = if let Some(mut entry) = self.map.first_entry() {
            let time_ns = *entry.key();
            if time_ns <= current_time {
                let set = entry.get_mut();
                let evt = set.pop_first().expect("inner set cannot be empty");
                if set.is_empty() {
                    entry.remove();
                }
                Some((time_ns, evt))
            } else {
                None
            }
        } else {
            None
        }?;

        if let TimedEvent::SignalEvt(id, _, _) = evt
            && self
                .signal_timers
                .get(&id)
                .is_some_and(|state| state.deadline == time_ns)
        {
            let state = self.signal_timers.get_mut(&id).unwrap();
            if state.interval == LogicalTime::ZERO {
                self.signal_timers.remove(&id);
            } else {
                state.deadline = time_ns + state.interval;
                let next_deadline = state.deadline;
                self.map.entry(next_deadline).or_default().insert(evt);
            }
        }
        Some((time_ns, evt))
    }

    /// Pop the next event unconditionally, if available.
    pub fn pop(&mut self) -> Option<(LogicalTime, TimedEvent)> {
        self.pop_if_before(LogicalTime::MAX)
    }

    /// Are there no timed events waiting?
    pub fn is_empty(&self) -> bool {
        // Here we rely on the invariant that there are no entries with empty sets on the RHS:
        self.map.is_empty()
    }

    /// Remove a specific thread from the set of those waiting on time to elapse.
    pub fn remove(&mut self, dettid: DetTid) {
        let mut to_remove: Option<LogicalTime> = None;
        let mut already_removed = false;
        for (time_key, set) in self.map.iter_mut() {
            let removed = set.remove(&TimedEvent::ThreadEvt(dettid));
            if removed {
                if already_removed {
                    panic!(
                        "invariant violation: multiple entries for dtid {} in TimedEvents",
                        dettid
                    );
                } else {
                    already_removed = true;
                }
            }
            // Cannot allow empty sets to remain:
            if set.is_empty() {
                to_remove = Some(*time_key);
            }
        }
        if let Some(time) = to_remove {
            let _ = self.map.remove(&time);
        }
    }

    /// Iterate over the entries in the TimedEvents collection
    pub fn iter(&self) -> impl Iterator<Item = (LogicalTime, TimedEvent)> + '_ {
        self.map
            .iter()
            .flat_map(|(key, set)| set.iter().map(|dtid| (*key, *dtid)))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn pid(n: i32) -> DetPid {
        DetPid::from_raw(n)
    }
    fn tid(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }
    fn at(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    /// Regression: an alarm that fires (is popped) must clear its timer
    /// bookkeeping so a subsequent alarm for the same process does not panic in
    /// `clear_old_alarm`. This reproduces the openssl-speed crash, where a
    /// SIGALRM fires and then the next timing round arms another alarm.
    #[test]
    fn reregister_after_fire_does_not_panic() {
        let mut ev = TimedEvents::default();
        let p = pid(100);

        assert_eq!(
            ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            None
        );

        // The alarm fires: the scheduler pops the due event.
        assert_eq!(
            ev.pop(),
            Some((
                at(1000),
                TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM),
            ))
        );
        assert!(ev.is_empty());

        // Arming a new alarm must see no stale previous alarm (the old one has
        // already fired) and must not panic.
        assert_eq!(
            ev.insert_alarm(at(2000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            None
        );
        assert_eq!(ev.len(), 1);
    }

    #[test]
    fn removing_alarm_preserves_other_process_at_same_deadline() {
        let mut ev = TimedEvents::default();
        let first_pid = pid(100);
        let second_pid = pid(200);
        let deadline = at(1_000);

        assert_eq!(
            ev.insert_alarm(
                deadline,
                first_pid,
                tid(101),
                Signal::SIGALRM,
                LogicalTime::ZERO,
            ),
            None
        );
        assert_eq!(
            ev.insert_alarm(
                deadline,
                second_pid,
                tid(201),
                Signal::SIGALRM,
                LogicalTime::ZERO,
            ),
            None
        );

        assert_eq!(
            ev.remove_alarm(first_pid),
            Some((deadline, LogicalTime::ZERO))
        );
        assert_eq!(
            ev.iter().collect::<Vec<_>>(),
            vec![(
                deadline,
                TimedEvent::SignalEvt(SignalTimerId::Alarm(second_pid), tid(201), Signal::SIGALRM,)
            )]
        );
        assert_eq!(
            ev.remove_alarm(second_pid),
            Some((deadline, LogicalTime::ZERO))
        );
        assert!(ev.is_empty());
    }

    /// Cancelling (`alarm(0)`) after a fire must be a no-op, not a panic.
    #[test]
    fn cancel_after_fire_does_not_panic() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO);
        let _ = ev.pop(); // fire
        assert_eq!(ev.remove_alarm(p), None);
        assert!(ev.is_empty());
    }

    /// Replacing a still-pending alarm reports the old target time and must not
    /// leave an empty set behind in `map` (which would break `is_empty()`).
    #[test]
    fn replace_pending_alarm_reports_old_and_leaves_no_empty_sets() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        assert_eq!(
            ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            None
        );
        assert_eq!(
            ev.insert_alarm(at(2000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO,),
            Some((at(1000), LogicalTime::ZERO))
        );
        // Only the replacement remains; the emptied 1000ns slot is gone.
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev.pop(),
            Some((
                at(2000),
                TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM),
            ))
        );
        assert!(ev.is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    #[test]
    fn periodic_alarm_rearms_at_its_interval() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        let event = TimedEvent::SignalEvt(SignalTimerId::Alarm(p), tid(100), Signal::SIGALRM);
        ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, at(250));

        assert_eq!(ev.pop_if_before(at(1000)), Some((at(1000), event)));
        assert_eq!(ev.pop_if_before(at(1249)), None);
        assert_eq!(ev.pop_if_before(at(1250)), Some((at(1250), event)));
        assert_eq!(ev.remove_alarm(p), Some((at(1500), at(250))));
        assert!(ev.is_empty());
    }

    /// A deterministic child-exit `SIGCHLD` must coexist with an `alarm(2)` on the
    /// *same* process at the *same* deadline without hitting the
    /// `insert_signal_timer` "already in set" panic (the collision that motivated
    /// the distinct `ChildExit` key), and must be delivered to the parent.
    #[test]
    fn child_exit_coexists_with_process_alarm_at_same_deadline() {
        let mut ev = TimedEvents::default();
        let parent = pid(100);
        let child = pid(200);
        let deadline = at(1_000);

        // Parent has an armed alarm...
        ev.insert_alarm(
            deadline,
            parent,
            tid(100),
            Signal::SIGALRM,
            LogicalTime::ZERO,
        );
        // ...and simultaneously reaps a child at the same logical time.
        ev.insert_child_exit(deadline, child, parent, tid(100));

        // Both are queued (no panic); popping yields the SIGALRM and the SIGCHLD.
        let first = ev.pop().expect("first event");
        let second = ev.pop().expect("second event");
        let popped = [first.1, second.1];
        assert!(popped.contains(&TimedEvent::SignalEvt(
            SignalTimerId::Alarm(parent),
            tid(100),
            Signal::SIGALRM
        )));
        assert!(popped.contains(&TimedEvent::SignalEvt(
            SignalTimerId::ChildExit { child, parent },
            tid(100),
            Signal::SIGCHLD
        )));
        assert!(ev.is_empty());

        // The alarm bookkeeping is untouched by the child-exit event: re-arming
        // sees no stale state (the fired alarm cleared itself) and does not panic.
        assert_eq!(
            ev.insert_alarm(
                at(2_000),
                parent,
                tid(100),
                Signal::SIGALRM,
                LogicalTime::ZERO
            ),
            None
        );
    }

    /// Two reports of the same child exit at the same deadline coalesce to a
    /// single `SIGCHLD`, matching Linux non-RT signal semantics; distinct
    /// children produce distinct events.
    #[test]
    fn child_exit_coalesces_duplicate_and_keeps_distinct_children() {
        let mut ev = TimedEvents::default();
        let parent = pid(100);
        let deadline = at(1_000);

        ev.insert_child_exit(deadline, pid(200), parent, tid(100));
        ev.insert_child_exit(deadline, pid(200), parent, tid(100)); // duplicate: coalesced
        ev.insert_child_exit(deadline, pid(201), parent, tid(100)); // distinct child

        assert_eq!(ev.iter().count(), 2);
        assert!(ev.iter().any(|(_, e)| e
            == TimedEvent::SignalEvt(
                SignalTimerId::ChildExit {
                    child: pid(200),
                    parent
                },
                tid(100),
                Signal::SIGCHLD
            )));
        assert!(ev.iter().any(|(_, e)| e
            == TimedEvent::SignalEvt(
                SignalTimerId::ChildExit {
                    child: pid(201),
                    parent
                },
                tid(100),
                Signal::SIGCHLD
            )));
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    #[test]
    fn posix_timer_does_not_replace_process_alarm() {
        let mut ev = TimedEvents::default();
        let p = pid(100);
        ev.insert_alarm(at(1000), p, tid(100), Signal::SIGALRM, LogicalTime::ZERO);
        ev.insert_posix_timer(at(500), p, tid(100), 7, Signal::SIGUSR1, LogicalTime::ZERO);

        assert_eq!(
            ev.pop(),
            Some((
                at(500),
                TimedEvent::SignalEvt(SignalTimerId::Posix(p, 7), tid(100), Signal::SIGUSR1,),
            ))
        );
        assert_eq!(ev.remove_alarm(p), Some((at(1000), LogicalTime::ZERO)));
        assert!(ev.is_empty());
    }
}
