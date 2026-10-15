/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The scheduler's half of `--pipe-wake-latency` (`crate::pipe_wake`,
//! <https://github.com/rrnewton/hermit/issues/4380>): restore the pollers an
//! action readied, and later move the acting thread behind them.
//!
//! Both run in RPCs of the acting thread while it holds the grant, so the
//! daemon waits in step 1 for its next request and no tentative pop is open:
//! the run queue can be changed directly, as `requeue_futex_waker` does.

use detcore_model::pid::DetPid;
use detcore_model::pid::DetTid;
use tracing::info;
use tracing::trace;

use super::Scheduler;
use super::runqueue::PollerRestore;
use crate::pipe_wake::ChannelId;
use crate::pipe_wake::ReadyOutcome;
use crate::pipe_wake::Wants;

impl Scheduler {
    /// Record that `process` received a signal whose default action ends it,
    /// in the turn that delivered it: from then on its threads are doomed
    /// although their removal from the scheduler comes at host-timed moments.
    /// In-memory only; no recording carries it.
    pub(crate) fn record_committed_fatal_signal(&mut self, process: DetPid) {
        self.fatal_signal_doomed.insert(process);
    }

    /// Whether a committed fatal signal doomed `process`. The only reader is
    /// the hand-off scan below.
    fn fatal_signal_doomed(&self, process: DetPid) -> bool {
        self.fatal_signal_doomed.contains(&process)
    }

    /// Whether `tid`'s process is doomed by a decision some committed turn
    /// made: a granted exit, a committed SIGKILL, an exec teardown in flight,
    /// or a delivered fatal signal. Such a thread leaves the scheduler at a
    /// host-timed moment, so the scan below must not depend on whether it
    /// already has.
    fn poller_is_doomed(&self, tid: DetTid) -> bool {
        let Some(process) = self.thread_tree.thread_to_leader.get(&tid).copied() else {
            return true;
        };
        self.thread_tree.granted_exits.contains(&process)
            || self.sigkilled_processes.contains(&process)
            || self.exec_teardowns.contains_key(&process)
            || self.fatal_signal_doomed(process)
    }

    /// An action of `actor` readied the pollers of `channel` that want
    /// `wants`. Restore the priority of each such queued poller, in run-queue
    /// order, skipping the actor and doomed pollers, make a retry it has filed
    /// runnable at its next selection, and remember them for the actor's
    /// hand-off. Answers whether the actor now has pollers to hand off to, and
    /// whether this report found the first of them since its last turn.
    ///
    /// A filed retry (`poll_attempt > 0`) is otherwise deferred once at its
    /// next selection, in step 4, and backed off again: the poller would lose
    /// the hand-off to the actor's yield. So the deferral's bookkeeping is done
    /// here instead (`note_sigalrm_polling_wait`, then
    /// `upgrade_polled_to_runnable`), which step 4 then finds done.
    ///
    /// Under a schedule replayer (`--replay-schedule-from`) the trace sets the
    /// run queue and reproduces the recorded switch to the poller, so nothing
    /// is restored or handed off: a hand-off's requeue and yield would add
    /// turns of the actor's own ahead of the backed-off poller, and each turn
    /// moves the guests' clock.
    ///
    /// Once a SIGKILL's victim could not be named, a doomed poller cannot be
    /// told from a live one, so the hand-off is inert for the rest of the run.
    pub(crate) fn ready_channel(
        &mut self,
        actor: DetTid,
        channel: ChannelId,
        wants: Wants,
    ) -> ReadyOutcome {
        if self.unresolved_sigkill {
            if !self.pipe_wake_inert_logged {
                self.pipe_wake_inert_logged = true;
                info!(
                    "[detcore] --pipe-wake-latency is inert for the rest of the run: a SIGKILL's \
                     victim could not be named"
                );
            }
            return ReadyOutcome::default();
        }
        if self.replayer.is_some() {
            return ReadyOutcome::default();
        }
        let queued: Vec<DetTid> = self.run_queue.tids().copied().collect();
        let mut readied = Vec::new();
        for tid in queued {
            if tid == actor || self.poller_is_doomed(tid) {
                continue;
            }
            let request = self
                .next_turns
                .get(&tid)
                .and_then(|turn| turn.req.try_read())
                .and_then(|request| request.as_ref().ok().cloned());
            let Some(request) = request.filter(|rs| rs.polls_channel == Some((channel, wants)))
            else {
                continue;
            };
            match self.run_queue.restore_poller_priority(tid) {
                PollerRestore::Restored | PollerRestore::NotBackedOff => {}
                PollerRestore::NotQueued => continue,
            }
            if request.poll_attempt > 0 {
                self.note_sigalrm_polling_wait(tid, &request);
                self.upgrade_polled_to_runnable(tid, &request);
            }
            readied.push(tid);
        }
        trace!(
            "[detcore] dtid {} readied {:?} of {:?}: pollers {:?}",
            actor, wants, channel, readied
        );
        let pending = self.pipe_handoff_pending.entry(actor).or_default();
        let was_empty = pending.is_empty();
        for tid in readied {
            if !pending.contains(&tid) {
                pending.push(tid);
            }
        }
        let has_pending = !pending.is_empty();
        if !has_pending {
            self.pipe_handoff_pending.remove(&actor);
        }
        ReadyOutcome {
            has_pending,
            starts_window: was_empty && has_pending,
        }
    }

    /// `actor`'s hand-off is due: if it readied any poller since its last
    /// turn, move it behind them, to the back of its priority band, and
    /// return true, so that it yields and they run first. Unconditional on
    /// whether those pollers are still queued: one a host-timed removal has
    /// taken is gone either way, and requeueing behind its absence gives the
    /// same schedule.
    pub(crate) fn hand_off_turn(&mut self, actor: DetTid) -> bool {
        if self.pipe_handoff_pending.remove(&actor).is_none() {
            return false;
        }
        if !self.run_queue.remove_tid(actor) {
            // Not queued: the actor is being torn down; there is nothing to
            // reorder.
            return false;
        }
        let pos = self.runqueue_push_back(actor);
        trace!(
            "[detcore] dtid {} hands the turn to the pollers it readied; requeued at {}",
            actor, pos
        );
        true
    }

    /// Any committed turn of `actor` is a hand-off in itself: it goes back
    /// behind the restored pollers at the end of that turn.
    pub(crate) fn forget_pipe_handoff(&mut self, actor: DetTid) {
        self.pipe_handoff_pending.remove(&actor);
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::fd::OpenFileId;

    use super::*;
    use crate::config::Config;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
    use crate::resources::Resources;
    use crate::scheduler::runqueue::DEFAULT_PRIORITY;
    use crate::scheduler::test::register_known_thread_for;
    use crate::types::MmId;

    const ACTOR: DetTid = DetTid::from_raw(1);
    const RUNNABLE: DetTid = DetTid::from_raw(2);
    const POLLER: DetTid = DetTid::from_raw(3);
    const SECOND_POLLER: DetTid = DetTid::from_raw(4);

    fn channel() -> ChannelId {
        ChannelId::Pipe(OpenFileId::new(ACTOR, 7))
    }

    /// Four one-thread processes, children of the actor's: the actor and a
    /// runnable thread queued at their normal priority, then two pollers of
    /// `channel()` for data, backed off after three failed attempts.
    fn scheduler() -> Scheduler {
        let mut scheduler = Scheduler::new(&Config::default());
        for tid in [ACTOR, RUNNABLE, POLLER, SECOND_POLLER] {
            scheduler.thread_tree.add_child(ACTOR, tid, true);
            register_known_thread_for(&mut scheduler, tid);
        }
        scheduler.runqueue_push_back(ACTOR);
        scheduler.runqueue_push_back(RUNNABLE);
        for poller in [POLLER, SECOND_POLLER] {
            poll(&mut scheduler, poller, Some((channel(), Wants::Data)));
            scheduler.run_queue.push_poller(poller, DEFAULT_PRIORITY, 3);
        }
        scheduler
    }

    fn poll(scheduler: &mut Scheduler, tid: DetTid, channel: Option<(ChannelId, Wants)>) {
        let mut request = Resources::new(tid);
        request.insert(ResourceID::InternalIOPolling, Permission::W);
        request.poll_attempt = 3;
        request.polls_channel = channel;
        scheduler.next_turns[&tid].req.put(Ok(request));
    }

    fn queue(scheduler: &Scheduler) -> Vec<DetTid> {
        scheduler.run_queue.tids().copied().collect()
    }

    /// Readying the channel restores both pollers, in run-queue order, and the
    /// hand-off moves the actor behind them, so they run before its next turn.
    #[test]
    fn readied_pollers_run_before_the_actor_after_its_hand_off() {
        let mut scheduler = scheduler();
        assert_eq!(queue(&scheduler), [ACTOR, RUNNABLE, POLLER, SECOND_POLLER]);
        assert_ne!(
            scheduler.run_queue.queued_priority(POLLER),
            Some(DEFAULT_PRIORITY)
        );

        let outcome = scheduler.ready_channel(ACTOR, channel(), Wants::Data);
        assert!(outcome.has_pending && outcome.starts_window, "{outcome:?}");
        for poller in [POLLER, SECOND_POLLER] {
            assert_eq!(
                scheduler.run_queue.queued_priority(poller),
                Some(DEFAULT_PRIORITY)
            );
            // The filed retry runs at its next selection, not deferred once
            // more by step 4 (Codex review of
            // https://github.com/rrnewton/hermit/pull/4424).
            let request = scheduler.next_turns[&poller]
                .req
                .try_read()
                .unwrap()
                .clone();
            assert_eq!(request.unwrap().poll_attempt, 0);
        }
        assert_eq!(
            scheduler.pipe_handoff_pending[&ACTOR],
            [POLLER, SECOND_POLLER]
        );
        // Readying again adds nobody twice, and starts no new window.
        let outcome = scheduler.ready_channel(ACTOR, channel(), Wants::Data);
        assert!(outcome.has_pending && !outcome.starts_window, "{outcome:?}");
        assert_eq!(
            scheduler.pipe_handoff_pending[&ACTOR],
            [POLLER, SECOND_POLLER]
        );

        assert!(scheduler.hand_off_turn(ACTOR));
        assert_eq!(queue(&scheduler), [RUNNABLE, POLLER, SECOND_POLLER, ACTOR]);
        assert!(!scheduler.pipe_handoff_pending.contains_key(&ACTOR));
        // A second hand-off has nothing to hand to.
        assert!(!scheduler.hand_off_turn(ACTOR));
    }

    /// Only a poll of the same channel for the readied want is readied: room
    /// readies no reader, and another pipe's data readies no reader of this
    /// one. Nothing is restored and the hand-off does nothing.
    #[test]
    fn another_want_or_channel_readies_nobody() {
        let other = ChannelId::Pipe(OpenFileId::new(ACTOR, 8));
        for (channel, wants) in [(channel(), Wants::Space), (other, Wants::Data)] {
            let mut scheduler = scheduler();
            assert!(!scheduler.ready_channel(ACTOR, channel, wants).has_pending);
            assert_ne!(
                scheduler.run_queue.queued_priority(POLLER),
                Some(DEFAULT_PRIORITY)
            );
            assert!(!scheduler.hand_off_turn(ACTOR));
            assert_eq!(queue(&scheduler), [ACTOR, RUNNABLE, POLLER, SECOND_POLLER]);
        }
    }

    /// A poller that a poll upgrade already restored still counts: the actor
    /// hands off to it.
    #[test]
    fn a_poller_at_its_normal_priority_counts() {
        let mut scheduler = Scheduler::new(&Config::default());
        for tid in [ACTOR, POLLER] {
            scheduler.thread_tree.add_child(ACTOR, tid, true);
            register_known_thread_for(&mut scheduler, tid);
            scheduler.runqueue_push_back(tid);
        }
        poll(&mut scheduler, POLLER, Some((channel(), Wants::Data)));
        assert!(
            scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        assert!(scheduler.hand_off_turn(ACTOR));
        assert_eq!(queue(&scheduler), [POLLER, ACTOR]);
    }

    /// The actor never hands off to itself, and a poll without a channel is
    /// not readied.
    #[test]
    fn the_actor_and_untagged_polls_are_skipped() {
        let mut scheduler = scheduler();
        poll(&mut scheduler, ACTOR, Some((channel(), Wants::Data)));
        poll(&mut scheduler, RUNNABLE, None);
        assert!(
            scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        assert_eq!(
            scheduler.pipe_handoff_pending[&ACTOR],
            [POLLER, SECOND_POLLER]
        );
    }

    /// A poller whose process a committed turn doomed is skipped, for each
    /// kind of doom, and so is a thread with no known process.
    #[test]
    fn doomed_pollers_are_skipped() {
        type Doom = fn(&mut Scheduler, DetPid);
        let doomers: [(&str, Doom); 4] = [
            ("granted exit", |s, p| {
                s.thread_tree.granted_exits.insert(p);
            }),
            ("committed SIGKILL", |s, p| {
                s.sigkilled_processes.insert(p);
            }),
            ("exec teardown", |s, p| {
                s.exec_teardowns.insert(
                    p,
                    super::super::ExecTeardown {
                        caller: p,
                        mm: MmId::initial(p),
                        siblings: Default::default(),
                        observed: Default::default(),
                    },
                );
            }),
            ("fatal signal", |s, p| s.record_committed_fatal_signal(p)),
        ];
        for (kind, doom) in doomers {
            let mut scheduler = scheduler();
            doom(&mut scheduler, POLLER);
            assert!(
                scheduler
                    .ready_channel(ACTOR, channel(), Wants::Data)
                    .has_pending,
                "{kind}"
            );
            assert_eq!(
                scheduler.pipe_handoff_pending[&ACTOR],
                [SECOND_POLLER],
                "{kind}"
            );
            assert_ne!(
                scheduler.run_queue.queued_priority(POLLER),
                Some(DEFAULT_PRIORITY),
                "{kind}: a doomed poller is not restored"
            );
        }
        let mut scheduler = scheduler();
        scheduler.thread_tree.thread_to_leader.remove(&POLLER);
        assert!(
            scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        assert_eq!(scheduler.pipe_handoff_pending[&ACTOR], [SECOND_POLLER]);
    }

    /// Once a SIGKILL's victim could not be named, the hand-off is inert.
    #[test]
    fn an_unresolved_sigkill_makes_the_hand_off_inert() {
        let mut scheduler = scheduler();
        scheduler.unresolved_sigkill = true;
        assert!(
            !scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        assert!(!scheduler.hand_off_turn(ACTOR));
        assert_eq!(queue(&scheduler), [ACTOR, RUNNABLE, POLLER, SECOND_POLLER]);
    }

    /// Under a schedule replayer the trace orders the turns: nothing is
    /// restored and the actor has nothing to hand off.
    #[test]
    fn a_schedule_replayer_makes_the_hand_off_inert() {
        let mut scheduler = scheduler();
        scheduler.replayer = Some(crate::scheduler::replayer::Replayer::new([]));
        assert!(
            !scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        assert_ne!(
            scheduler.run_queue.queued_priority(POLLER),
            Some(DEFAULT_PRIORITY)
        );
        assert!(!scheduler.hand_off_turn(ACTOR));
        assert_eq!(queue(&scheduler), [ACTOR, RUNNABLE, POLLER, SECOND_POLLER]);
    }

    /// The hand-off does not depend on whether the pollers are still queued:
    /// one a host-timed removal has taken gives the same requeue.
    #[test]
    fn the_hand_off_is_unconditional_on_the_pollers_still_being_queued() {
        let mut scheduler = scheduler();
        assert!(
            scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        assert!(scheduler.run_queue.remove_tid(POLLER));
        assert!(scheduler.run_queue.remove_tid(SECOND_POLLER));
        assert!(scheduler.hand_off_turn(ACTOR));
        assert_eq!(queue(&scheduler), [RUNNABLE, ACTOR]);
    }

    /// A committed turn of the actor is its hand-off: the pending set is
    /// forgotten and a later hand-off does nothing.
    #[test]
    fn a_committed_turn_forgets_the_hand_off() {
        let mut scheduler = scheduler();
        assert!(
            scheduler
                .ready_channel(ACTOR, channel(), Wants::Data)
                .has_pending
        );
        scheduler.forget_pipe_handoff(ACTOR);
        assert!(!scheduler.hand_off_turn(ACTOR));
        assert_eq!(queue(&scheduler), [ACTOR, RUNNABLE, POLLER, SECOND_POLLER]);
        // After that turn, whatever request it granted, the next readying
        // starts a new window: the thread's latency restarts from it.
        let outcome = scheduler.ready_channel(ACTOR, channel(), Wants::Data);
        assert!(outcome.has_pending && outcome.starts_window, "{outcome:?}");
    }
}
