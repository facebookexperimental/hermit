/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A fair order for the threads of a run past the end of its replayed
//! schedule, under `--replay-continue-fairly`
//! (`Config::replay_continue_fairly`).
//!
//! Past the end of a replayed schedule every thread has the default priority,
//! and threads of one priority take turns in queue order. A committing call
//! (a write, a futex wait) sends its thread to the back of the queue, while a
//! thread that only computes or reads the clock keeps the CPU until its
//! timeslice ends. So beside a spinning thread a thread that makes committing
//! calls runs one of them per timeslice: a pipeline test whose recording ran
//! in 24 s reached 5 of its 23 restarts in 600 s there.
//!
//! With the option, the threads queued in round-robin order in each band
//! are ordered by the virtual time each has run since the continuation
//! began, least first, as Linux's fair scheduler orders runnable tasks by
//! their virtual run time. A thread's run time is what its own clock has
//! advanced since the continuation first saw it; sleeps and timed waits do
//! not advance a thread's clock. A thread seen for the first time, or
//! runnable again after it was not, starts at no less than the least run time
//! of the threads that were runnable (the floor), so a new or long-blocked
//! thread does not keep the CPU until it has caught up with the others; a
//! thread whose clock the scheduler has not seen yet is placed at the floor.
//! Its selections are still recorded, and its first reported work is charged
//! from the clock inherited at creation, whichever pass first sees that clock.
//! Equal run times go to the thread selected least recently, and a thread
//! runnable again counts as never selected, so a thread raised to the floor
//! goes ahead of the thread that holds it.
//!
//! That is all it changes. A selected thread still runs until its slice ends
//! as before (`--max-timeslice`): there is no preemption on wakeup, no weight
//! and no credit for sleeping. Entries pushed to the front of their band (a
//! timed wake, an eager readmission) and pollers keep their places, ahead of
//! the ordered entries. Nothing comes from the recording and nothing is drawn
//! at random: the order is a function of the threads' clocks, of which
//! threads are runnable and of the order threads were selected in.

use std::collections::BTreeMap;

use crate::types::DetTid;
use crate::types::LogicalTime;

/// A parked thread's reported clock and its deterministic startup baseline.
#[derive(Debug, Clone, Copy)]
pub(super) struct Clock {
    pub(super) current: LogicalTime,
    pub(super) inherited: LogicalTime,
}

/// One thread's account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Account {
    /// The last reported clock, or None until the first startup RPC arrives.
    clock: Option<LogicalTime>,
    /// Virtual nanoseconds the thread has run since the continuation began,
    /// raised to the floor when it became runnable.
    run_time: u64,
    /// Whether the thread was runnable then.
    runnable: bool,
    /// When the thread was last selected, in selections since the
    /// continuation began; 0 if never.
    last_selected: u64,
}

/// The run time of every thread the continuation has seen.
#[derive(Debug, Default)]
pub(super) struct FairContinuation {
    accounts: BTreeMap<DetTid, Account>,
    /// The least run time of a runnable thread, never decreasing.
    floor: u64,
    /// Selections since the continuation began.
    selections: u64,
}

impl FairContinuation {
    /// Bring the accounts up to date with `clocks`, one slot per registered
    /// thread, and return the sort key of each thread in `runnable`
    /// (sorted by thread): its run time, then when it was last selected.
    /// Drop accounts only when registration ends, not while waiting for the
    /// first clock. A new thread without a clock starts at the floor, and
    /// its first report charges work since its inherited startup clock.
    ///
    /// Established runnable threads have published their request's clock, so
    /// charging its advance before raising a newly runnable thread to the
    /// floor charges work done before it blocked, not after. Startup admission
    /// can precede the first request: that pending account keeps its selections
    /// and uses the inherited baseline when a later snapshot sees the clock.
    pub(super) fn run_times(
        &mut self,
        clocks: &BTreeMap<DetTid, Option<Clock>>,
        runnable: &[DetTid],
    ) -> Vec<(DetTid, (u64, u64))> {
        let floor = self.floor;
        self.accounts.retain(|tid, _| clocks.contains_key(tid));
        for (&tid, &clock) in clocks {
            let is_runnable = runnable.binary_search(&tid).is_ok();
            let account = self.accounts.entry(tid).or_insert(Account {
                clock: clock.map(|reported| reported.current),
                run_time: floor,
                runnable: false,
                last_selected: 0,
            });
            if let Some(reported) = clock {
                // A clock never goes back; an exec reload restores the same
                // one. A delayed first report includes the child's first
                // turn, which must be charged exactly as with an early RPC.
                let previous = account.clock.unwrap_or(reported.inherited);
                let ran = reported
                    .current
                    .as_nanos()
                    .saturating_sub(previous.as_nanos());
                account.run_time = account.run_time.saturating_add(ran);
                account.clock = Some(reported.current);
            }
            if is_runnable && !account.runnable {
                account.run_time = account.run_time.max(floor);
                // A thread back in turn has waited: it goes ahead of the
                // threads it ties with, whichever way it was woken (a wake
                // pushed to the front was selected, and so noted, before it
                // got here).
                account.last_selected = 0;
            }
            account.runnable = is_runnable;
        }
        let keys: Vec<(DetTid, (u64, u64))> = runnable
            .iter()
            .map(|tid| {
                let key = self.accounts.get(tid).map_or((floor, 0), |account| {
                    (account.run_time, account.last_selected)
                });
                (*tid, key)
            })
            .collect();
        if let Some(least) = keys.iter().map(|(_, (run_time, _))| *run_time).min() {
            self.floor = self.floor.max(least);
        }
        keys
    }

    /// Note that `tid` was selected and had a turn, including when its first
    /// clock was absent from the preceding snapshot. The turn that exhausted
    /// replay can precede the first account snapshot and is not charged.
    pub(super) fn note_selected(&mut self, tid: DetTid) {
        self.selections += 1;
        if let Some(account) = self.accounts.get_mut(&tid) {
            account.last_selected = self.selections;
        }
    }

    /// A non-leader exec moved `from`'s clock to `to`, the leader's thread
    /// id; its account goes with it, replacing the old leader's.
    pub(super) fn reassign(&mut self, from: DetTid, to: DetTid) {
        if from == to {
            return;
        }
        match self.accounts.remove(&from) {
            Some(account) => {
                self.accounts.insert(to, account);
            }
            None => {
                self.accounts.remove(&to);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clocks(entries: &[(DetTid, u64)]) -> BTreeMap<DetTid, Option<Clock>> {
        entries
            .iter()
            .map(|&(tid, ns)| {
                (
                    tid,
                    Some(Clock {
                        current: LogicalTime::from_nanos(ns),
                        inherited: LogicalTime::ZERO,
                    }),
                )
            })
            .collect()
    }

    /// A thread's run time is what its clock advanced since it was first
    /// seen; the thread that ran less comes first.
    #[test]
    fn the_thread_that_ran_less_comes_first() {
        let (writer, spinner) = (DetTid::from_raw(3), DetTid::from_raw(5));
        let mut fair = FairContinuation::default();
        assert_eq!(
            fair.run_times(
                &clocks(&[(writer, 1_000), (spinner, 1_000)]),
                &[writer, spinner]
            ),
            vec![(writer, (0, 0)), (spinner, (0, 0))]
        );
        // The spinner ran a 200 ms slice, the writer half a millisecond.
        assert_eq!(
            fair.run_times(
                &clocks(&[(writer, 501_000), (spinner, 200_001_000)]),
                &[writer, spinner]
            ),
            vec![(writer, (500_000, 0)), (spinner, (200_000_000, 0))]
        );
    }

    /// A thread seen for the first time, or runnable again after it was not,
    /// starts at the least run time of the threads that were runnable, not
    /// at nothing; one that ran more keeps its own.
    #[test]
    fn a_new_or_woken_thread_starts_at_the_floor() {
        let (writer, spinner, late) = (
            DetTid::from_raw(3),
            DetTid::from_raw(5),
            DetTid::from_raw(7),
        );
        let mut fair = FairContinuation::default();
        fair.run_times(&clocks(&[(writer, 0), (spinner, 0)]), &[writer, spinner]);
        fair.run_times(
            &clocks(&[(writer, 30_000_000), (spinner, 50_000_000)]),
            &[writer, spinner],
        );
        // The writer blocks; the spinner runs on.
        fair.run_times(
            &clocks(&[(writer, 30_000_000), (spinner, 90_000_000)]),
            &[spinner],
        );
        // The writer wakes and a new thread starts: both at the floor, the
        // spinner's 90 ms, not at 30 ms or nothing.
        assert_eq!(
            fair.run_times(
                &clocks(&[(writer, 30_000_000), (spinner, 90_000_000), (late, 5)]),
                &[writer, spinner, late]
            ),
            vec![
                (writer, (90_000_000, 0)),
                (spinner, (90_000_000, 0)),
                (late, (90_000_000, 0))
            ]
        );
        // A registered thread without a clock yet keeps an account at the
        // floor; accounts of threads whose registration ended are dropped.
        let pending = DetTid::from_raw(9);
        let mut registered = clocks(&[(writer, 30_000_001)]);
        registered.insert(pending, None);
        assert_eq!(
            fair.run_times(&registered, &[writer, pending]),
            vec![(writer, (90_000_001, 0)), (pending, (90_000_000, 0))]
        );
        assert_eq!(fair.accounts.len(), 2);
        assert_eq!(fair.accounts[&pending].clock, None);
        assert!(!fair.accounts.contains_key(&spinner));
        assert!(!fair.accounts.contains_key(&late));
        fair.note_selected(pending);
        // Reporting the inherited clock contributes no work and keeps that
        // selection, even though the inherited duration is greater than zero.
        registered.insert(
            pending,
            Some(Clock {
                current: LogicalTime::from_nanos(1_000),
                inherited: LogicalTime::from_nanos(1_000),
            }),
        );
        assert_eq!(
            fair.run_times(&registered, &[writer, pending]),
            vec![(writer, (90_000_001, 0)), (pending, (90_000_000, 1))]
        );
    }

    /// Equal run times go to the thread selected least recently, a thread
    /// runnable again counts as never selected, and an exec moves the
    /// caller's account to the leader's thread id.
    #[test]
    fn ties_go_to_the_thread_selected_least_recently() {
        let (main, writer) = (DetTid::from_raw(3), DetTid::from_raw(5));
        let mut fair = FairContinuation::default();
        fair.run_times(&clocks(&[(main, 0), (writer, 0)]), &[main, writer]);
        fair.note_selected(writer);
        fair.note_selected(main);
        assert_eq!(
            fair.run_times(&clocks(&[(main, 0), (writer, 0)]), &[main, writer]),
            vec![(main, (0, 2)), (writer, (0, 1))]
        );
        // The writer blocks while main runs, then wakes (through the front of
        // the queue, so it was selected last) and ties with main at the floor:
        // it goes first.
        fair.run_times(&clocks(&[(main, 10), (writer, 0)]), &[main]);
        fair.note_selected(writer);
        assert_eq!(
            fair.run_times(&clocks(&[(main, 10), (writer, 0)]), &[main, writer]),
            vec![(main, (10, 2)), (writer, (10, 0))]
        );
        fair.reassign(writer, main);
        assert_eq!(
            fair.run_times(&clocks(&[(main, 17)]), &[main]),
            vec![(main, (27, 0))]
        );
    }
}
