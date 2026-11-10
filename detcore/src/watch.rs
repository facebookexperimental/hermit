/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit run --watch`: arming the guest threads' hardware watchpoints.
//!
//! Debug registers are per thread and not inherited, so every thread arms its
//! own. The scheduler decides when the watches are due: once it has recorded
//! `--watch-from` events, its epoch is 1. It tells a thread the epoch in the
//! replies a thread already receives at its stops (a [`WatchOrder`] on every
//! `TraceSchedEvent` reply and on every reply after the thread waited for its
//! turn), and the thread arms at the first such stop where Reverie accepts
//! the watches. The thread whose event reached `--watch-from` arms at that
//! event's reply; every other thread arms before it next runs guest code,
//! because it resumes only through one of those replies.
//!
//! A thread reports its state with each event it records ([`WatchStatus`]),
//! so the scheduler can say in the trace which threads armed and which never
//! did. Each watch asks Reverie for the stacks of its first
//! [`WATCH_HITS_WITH_STACKS`] hits on the thread, a budget Reverie spends at
//! the hits, so a word the thread writes in a loop costs no unwinding after
//! that. Hits reach `GlobalTool::on_watchpoint_hit`, which only appends to
//! the access trace (`crate::access_trace`), unless the hit's SIGTRAP may
//! have changed the guest ([`hit_refusal`]).
//!
//! Nothing here sends a request, moves virtual time or touches the guest
//! beyond its debug registers.

use detcore_model::access_trace::WATCH_HITS_WITH_STACKS;
use detcore_model::watch::WatchAccess;
use reverie::Guest;
use reverie::WatchError;
use reverie::WatchKind;
use reverie::WatchSpec;
use serde::Deserialize;
use serde::Serialize;

use crate::Detcore;
use crate::RecordOrReplay;

/// What the scheduler tells a thread at a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchOrder {
    /// 0 until the watches are due, then 1.
    pub epoch: u64,
}

/// A thread's watch state, as it reports it with each event it records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum WatchStatus {
    /// It has not been told the watches are due.
    #[default]
    Pending,
    /// Its last attempt was refused: it blocked SIGTRAP.
    SigtrapBlocked,
    /// Its last attempt was refused: its process ignored SIGTRAP.
    SigtrapIgnored,
    /// Its watches are armed.
    Armed,
}

/// A thread's own watch state. A thread starts without watches, whatever its
/// creator had: `Clone` gives a fresh one, as Linux gives a new thread clear
/// debug registers.
#[derive(Debug, Default)]
pub struct WatchThread {
    /// The epoch it armed for, 0 if it has not.
    pub(crate) armed_epoch: u64,
    /// The epoch the scheduler last told it, whether it armed or not. It
    /// survives exec, so the new image arms at its first stop.
    pub(crate) due_epoch: u64,
    pub(crate) status: WatchStatus,
}

impl Clone for WatchThread {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl WatchThread {
    /// What the thread reports with an event it records.
    pub fn status(&self) -> WatchStatus {
        self.status
    }

    /// The thread's image was replaced: Linux cleared its debug registers.
    /// What the scheduler told it stays.
    pub fn exec(&mut self) {
        *self = Self {
            due_epoch: self.due_epoch,
            ..Self::default()
        };
    }
}

/// Reverie's spelling of `--watch`'s watches, each asking for the stacks of
/// its first [`WATCH_HITS_WITH_STACKS`] hits on the thread.
pub fn specs(watches: &[detcore_model::watch::WatchArg]) -> Vec<WatchSpec> {
    let backtraces = u32::try_from(WATCH_HITS_WITH_STACKS).expect("a small budget");
    watches
        .iter()
        .map(|watch| WatchSpec {
            addr: watch.addr,
            len: watch.len,
            kind: match watch.access {
                WatchAccess::Write => WatchKind::Write,
                WatchAccess::ReadWrite => WatchKind::ReadWrite,
            },
            backtraces,
        })
        .collect()
}

/// Why the run stops at `hit`, if it must: Reverie says the hit's SIGTRAP
/// may have left the guest a SIGTRAP mask bit or action it did not set (it
/// reset an ignored or caught-while-blocked SIGTRAP's action to the default,
/// or the backend could not tell its state). The guest could then run
/// differently than it would without the watch, and a watch must only
/// observe.
pub fn hit_refusal(hit: &reverie::WatchHit) -> Option<String> {
    hit.sigtrap.may_have_changed_guest().then(|| {
        format!(
            "hermit: --watch refused: a watchpoint hit on thread {} (watch {} at {:#x}, \
             instruction {:#x}) may have changed the guest's SIGTRAP state ({:?}): the kernel \
             reports a hit by forcing a SIGTRAP, which unblocks a blocked SIGTRAP and resets an \
             ignored one, and Reverie could not put it back. The run would no longer be the run \
             without the watch. Run without --watch, or watch where the guest does not block \
             or ignore SIGTRAP.",
            hit.tid, hit.slot, hit.spec.addr, hit.ip, hit.sigtrap,
        )
    })
}

/// At the post-exec stop, the new image's first: Linux cleared the thread's
/// debug registers, and a thread the scheduler had told the watches are due
/// arms again now ([`adopt`]), before the image runs. Its next order can come
/// only after the image has run, at its first recorded event or turn.
pub fn rearm_after_exec<G, T>(guest: &mut G)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let epoch = guest.thread_state().watch.due_epoch;
    if epoch > 0 {
        adopt(guest, WatchOrder { epoch });
    }
}

/// Act on the scheduler's `order` at this stop ([`try_adopt`]), and stop the
/// run if the thread cannot be watched as asked.
pub fn adopt<G, T>(guest: &mut G, order: WatchOrder)
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if let Err(refusal) = try_adopt(guest, order) {
        crate::scheduler::exit_on_scheduler_refusal(&refusal);
    }
}

/// Arm the thread's watches if `order` says they are due and it has not. A
/// thread that blocks or ignores SIGTRAP stays as it is and tries again at
/// its next stop. A thread whose hits could move a timer event, or whose
/// watches cannot be written, is refused: a watch must not change the
/// schedule, and a run that silently watched nothing would mislead.
pub fn try_adopt<G, T>(guest: &mut G, order: WatchOrder) -> Result<(), String>
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    if guest.config().watch.is_empty() || order.epoch == 0 {
        return Ok(());
    }
    let state = &mut guest.thread_state_mut().watch;
    state.due_epoch = state.due_epoch.max(order.epoch);
    if order.epoch <= state.armed_epoch {
        return Ok(());
    }
    let tid = guest.thread_state().dettid;
    if !guest.watch_hits_keep_timer_events() {
        return Err(format!(
            "hermit: --watch refused: a watchpoint hit on thread {tid} could move its next \
             preemption, because this host's precise timer has no overflow records mapped (a \
             PREEMPT_RT kernel, or perf's locked-memory budget ran out), so the run would not \
             follow its schedule. Run without --watch, or raise the perf memory budget \
             (kernel.perf_event_mlock_kb)."
        ));
    }
    let specs = specs(&guest.config().watch);
    let result = guest.set_watchpoints(&specs);
    let state = &mut guest.thread_state_mut().watch;
    match result {
        Ok(()) => {
            state.armed_epoch = order.epoch;
            state.status = WatchStatus::Armed;
        }
        Err(WatchError::SigtrapBlocked) => state.status = WatchStatus::SigtrapBlocked,
        Err(WatchError::SigtrapIgnored) => state.status = WatchStatus::SigtrapIgnored,
        Err(error) => {
            return Err(format!(
                "hermit: --watch refused: thread {tid} could not arm its watches: {error}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use detcore_model::watch::WatchArg;

    use super::*;

    #[test]
    fn watches_become_reverie_specs_and_a_new_thread_starts_unarmed() {
        let watches: Vec<WatchArg> = vec![
            "0x404010:8:w".parse().unwrap(),
            "0x404020:1:rw".parse().unwrap(),
        ];
        assert_eq!(
            specs(&watches),
            vec![
                WatchSpec {
                    addr: 0x404010,
                    len: 8,
                    kind: WatchKind::Write,
                    backtraces: 64,
                },
                WatchSpec {
                    addr: 0x404020,
                    len: 1,
                    kind: WatchKind::ReadWrite,
                    backtraces: 64,
                },
            ]
        );

        let armed = WatchThread {
            armed_epoch: 1,
            due_epoch: 1,
            status: WatchStatus::Armed,
        };
        let child = armed.clone();
        assert_eq!(
            (child.armed_epoch, child.due_epoch, child.status),
            (0, 0, WatchStatus::Pending)
        );
        let mut execed = armed;
        execed.exec();
        assert_eq!(
            (execed.armed_epoch, execed.due_epoch, execed.status()),
            (0, 1, WatchStatus::Pending)
        );
    }

    /// A hit whose SIGTRAP may have changed the guest stops the run, naming
    /// the thread and what Reverie says the SIGTRAP did; one that changed
    /// nothing, or that Reverie put back, does not.
    #[test]
    fn a_hit_that_may_have_changed_the_guest_stops_the_run() {
        use reverie::SigtrapAtHit;
        let hit = |sigtrap| {
            reverie::WatchHit::new(
                reverie::Pid::from_raw(7),
                1,
                specs(&["0x404010:8:w".parse().unwrap()])[0],
                0x401005,
                reverie::HitTiming::AfterAccess,
                None,
                sigtrap,
            )
        };
        assert_eq!(hit_refusal(&hit(SigtrapAtHit::Unchanged)), None);
        assert_eq!(hit_refusal(&hit(SigtrapAtHit::Reblocked)), None);
        for sigtrap in [SigtrapAtHit::ActionReset, SigtrapAtHit::Unknown] {
            let refusal = hit_refusal(&hit(sigtrap)).expect("refused");
            assert!(
                refusal.contains("a watchpoint hit on thread 7 (watch 1 at 0x404010")
                    && refusal.contains(&format!("SIGTRAP state ({sigtrap:?})")),
                "{refusal}"
            );
        }
    }
}
