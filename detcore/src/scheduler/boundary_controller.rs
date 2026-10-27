/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Controller integration for the single boundary/event cursor and its
//! authenticated grant lookahead.

use detcore_model::futex::MmId;
use detcore_model::schedule::boundary::Boundary;
use detcore_model::schedule::boundary::Record;

use super::ConsumeResult;
use super::LogicalTime;
use super::REPLAY_DEFERRED_PRIORITY;
use super::REPLAY_FOREGROUND_PRIORITY;
use super::Scheduler;
use super::replayer::ReplayAction;
use super::replayer::Replayer;
use crate::types::DetTid;

impl Scheduler {
    /// Look ahead only for the scheduler's authenticated live incarnation.
    /// This does not consume a record, choose an owner, or commit a turn.
    pub(crate) fn boundary_budget_for_live_owner(
        &self,
        tid: DetTid,
    ) -> anyhow::Result<Option<crate::slice_boundary::grant::Budget>> {
        let Some(replayer) = &self.boundary_replayer else {
            return Ok(None);
        };
        let mm = self.thread_mm(tid).ok_or_else(|| {
            anyhow::anyhow!(
                "slice-boundary protocol refusal: grant has no registered address space"
            )
        })?;
        let owner = self
            .boundary_owners
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("slice-boundary protocol refusal: replay has no lifecycle registry")
            })?
            .owner(tid, mm)?;
        Ok(replayer.budget_for(owner))
    }

    pub(super) fn prepare_boundary_grant(&mut self, tid: DetTid) -> anyhow::Result<()> {
        if let Some(budget) = self.boundary_budget_for_live_owner(tid)? {
            let mm = self
                .thread_mm(tid)
                .expect("lookahead authenticated this address space");
            self.boundary_budgets.queue(mm, budget)?;
        }
        Ok(())
    }

    /// Consume the annotation before publishing its unchanged resource map.
    /// Neither a missing owner nor an invalid record can file that request.
    pub(crate) fn accept_slice_boundary(
        &mut self,
        tid: DetTid,
        mm: MmId,
        boundary: &Boundary,
    ) -> anyhow::Result<super::MaybePrintStack> {
        let owners = self.boundary_owners.as_ref().ok_or_else(|| {
            anyhow::anyhow!("slice-boundary protocol refusal: annotation outside its protocol")
        })?;
        owners.validate_boundary(tid, mm, boundary)?;
        let recording = match &self.preemption_writer {
            Some(writer) if writer.records_slice_boundaries() => true,
            Some(_) => {
                anyhow::bail!("slice-boundary protocol refusal: annotation has a legacy writer")
            }
            None => false,
        };
        if !recording && self.boundary_replayer.is_none() {
            anyhow::bail!(
                "slice-boundary protocol refusal: annotation has no recorder or replay cursor"
            );
        }
        let next_count = if recording {
            let next = self.recorded_event_count.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!("slice-boundary protocol refusal: stored index overflow")
            })?;
            if self
                .preemption_writer
                .as_ref()
                .unwrap()
                .next_boundary_index()
                .and_then(|index| u64::try_from(index).ok())
                != Some(self.recorded_event_count)
            {
                anyhow::bail!("slice-boundary protocol refusal: inconsistent recording index");
            }
            if self.boundary_replayer.is_none()
                && let Some(iter) = &mut self.stacktrace_events
                && let Some((requested, event, _)) = iter.peek()
                && *requested == self.recorded_event_count
                && event.is_some()
            {
                anyhow::bail!(
                    "slice-boundary protocol refusal: ordinary event selector names a marker"
                );
            }
            Some(next)
        } else {
            None
        };
        let mut stack = None;
        if self.boundary_replayer.is_some() {
            let consumed = self.consume_boundary_record(&Record::TimesliceEnd(boundary.clone()))?;
            assert!(
                consumed.keep_running,
                "a boundary must use its original resource handoff"
            );
            stack = consumed.print_stack;
        }
        if recording {
            let index = self
                .preemption_writer
                .as_mut()
                .unwrap()
                .insert_slice_boundary(boundary.clone())
                .map_err(anyhow::Error::msg)?;
            assert_eq!(index as u64, self.recorded_event_count);
            self.recorded_event_count = next_count.unwrap();
            if self.boundary_replayer.is_none()
                && let Some(iter) = &mut self.stacktrace_events
                && let Some((requested, event, path)) = iter.peek()
                && *requested == index as u64
            {
                assert!(
                    event.is_none(),
                    "event selector was validated before consumption"
                );
                stack = Some(path.clone());
                iter.next();
            }
        }
        self.boundary_owners
            .as_mut()
            .unwrap()
            .commit_boundary(boundary);
        Ok(stack)
    }

    pub(crate) fn has_schedule_replay(&self) -> bool {
        assert!(
            self.replayer.is_none() || self.boundary_replayer.is_none(),
            "two replay cursors cannot own the same schedule"
        );
        self.replayer.is_some() || self.boundary_replayer.is_some()
    }

    pub(super) fn replay_has_records_left(&self) -> bool {
        assert!(self.replayer.is_none() || self.boundary_replayer.is_none());
        match &self.boundary_replayer {
            Some(replayer) => replayer.has_records_left(),
            None => self
                .replayer
                .as_ref()
                .is_some_and(Replayer::has_events_left),
        }
    }

    pub(super) fn replay_next_resume(&self) -> Option<(DetTid, Option<LogicalTime>)> {
        assert!(self.replayer.is_none() || self.boundary_replayer.is_none());
        match &self.boundary_replayer {
            // Exec uses the current stored cursor, with no reconstructed
            // logical-duration heuristic and no cursor/clock/turn mutation.
            Some(replayer) => replayer.next_thread().map(|tid| (tid, None)),
            None => self.replayer.as_ref().and_then(Replayer::next_resume),
        }
    }

    pub(super) fn consume_boundary_record(
        &mut self,
        observed: &Record,
    ) -> anyhow::Result<ConsumeResult> {
        assert!(self.replayer.is_none());
        let replayer = self
            .boundary_replayer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no slice-boundary replay cursor"))?;
        let index = replayer.stored_index();
        let action = replayer.observe(observed)?;
        let complete = replayer.stored_index() != index;
        // A partial branch has not reached its stored endpoint. A later event
        // that happens to look identical cannot steal a requested stacktrace.
        let print_stack = if complete {
            self.boundary_stacktrace_events.remove(&index)
        } else {
            None
        };
        self.release_replay_priorities_if_exhausted();
        let keep_running = match action {
            ReplayAction::Continue(None) => true,
            ReplayAction::ContextSwitch(now, next, None) => {
                let current = match observed {
                    Record::Event(event) => event.dettid,
                    Record::TimesliceEnd(boundary) => boundary.identity.owner.tid,
                };
                assert_ne!(
                    current, next,
                    "same-owner annotations must not rotate the queue"
                );
                self.requeue_with_new_priority(current, REPLAY_DEFERRED_PRIORITY);
                self.requeue_with_new_priority(next, REPLAY_FOREGROUND_PRIORITY);
                // A timeslice-end marker also switches before its request, but
                // that request resumes guest code: only a syscall prehook hands
                // its call a turn (`replay_prehook_handoff`).
                let at_syscall_prehook = matches!(
                    observed,
                    Record::Event(event) if super::observed_is_syscall_prehook(event)
                );
                self.note_replay_switch(current, next, at_syscall_prehook);
                // The expectation rides the existing grant. No old leader's
                // legacy duration may leak into this new-format continuation.
                self.timeslices.remove(&next);
                !now
            }
            _ => unreachable!("the boundary cursor cannot resync or infer a logical duration"),
        };
        Ok(ConsumeResult {
            keep_running,
            print_stack,
            event_ix: index as u64,
            timeslice_remaining: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::SchedEvent;
    use detcore_model::schedule::boundary::Boundary;
    use detcore_model::schedule::boundary::Cause;
    use detcore_model::schedule::boundary::Entry;
    use detcore_model::schedule::boundary::Identity;
    use detcore_model::schedule::boundary::Owner;
    use detcore_model::schedule::boundary::Phase;
    use detcore_model::schedule::boundary::Schedule;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::config::Config;
    use crate::ivar::Ivar;
    use crate::preemptions::PreemptionWriter;
    use crate::resources::Permission;
    use crate::resources::ResourceID;
    use crate::resources::Resources;
    use crate::scheduler::ThreadNextTurn;
    use crate::scheduler::boundary_replayer::BoundaryReplayer;
    use crate::scheduler::runqueue::DEFAULT_PRIORITY;
    use crate::types::SyscallPhase;

    fn marker(tid: i32, slice: u64) -> Record {
        Record::TimesliceEnd(Boundary {
            identity: Identity {
                owner: Owner {
                    tid: DetTid::from_raw(tid),
                    thread_generation: 0,
                    birth_tid: DetTid::from_raw(tid),
                    birth_generation: 0,
                    address_space_creator: DetTid::from_raw(tid),
                    address_space_generation: 0,
                },
                cause: Cause::SoftwareCheckin,
                phase: Phase::Posthook,
                entry: Entry::Syscall {
                    syscall: Sysno::getpid,
                    occurrence: 1,
                },
                committed_rcbs: 0,
                checkin: 1,
                slice,
            },
            count: 1,
            start_rip: None,
            end_rip: None,
            end_time: None,
        })
    }
    fn scheduler(records: Vec<Record>) -> Scheduler {
        let mut scheduler = Scheduler::new(&Config::default());
        scheduler.boundary_replayer = Some(BoundaryReplayer::new(Schedule::new(records).unwrap()));
        scheduler
    }

    #[test]
    fn boundary_constructor_preserves_formats_and_authenticates_lookahead() {
        use crate::preemptions::PreemptionRecord;
        use crate::preemptions::boundary_recording::BoundaryRecording;
        use crate::preemptions::boundary_recording::Recording;
        let directory = tempfile::tempdir().unwrap();
        let tid = DetTid::from_raw(3);
        let mm = MmId::initial(tid);
        let schedule = Schedule::new(vec![marker(3, 1)]).unwrap();
        let path = directory.path().join("recording.json");
        Recording::Boundaries(
            BoundaryRecording::new(PreemptionRecord::from_sched_events(vec![]), schedule).unwrap(),
        )
        .write(&path)
        .unwrap();
        let config = Config {
            sequentialize_threads: true,
            replay_schedule_from: Some(path.clone()),
            record_preemptions: true,
            strict_schedule_boundaries: true,
            stacktrace_event: vec![(0, Some("marker.stack".into()))],
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        assert!(scheduler.replayer.is_none());
        assert!(
            scheduler
                .preemption_writer
                .as_ref()
                .unwrap()
                .records_slice_boundaries()
        );
        assert_eq!(
            scheduler.boundary_stacktrace_events.get(&0),
            Some(&Some("marker.stack".into()))
        );
        assert!(scheduler.boundary_budget_for_live_owner(tid).is_err());
        scheduler.record_thread_mm(tid, mm);
        assert!(scheduler.boundary_budget_for_live_owner(tid).is_err());
        scheduler
            .boundary_owners
            .as_mut()
            .unwrap()
            .fresh(tid, mm)
            .unwrap();
        let budget = scheduler
            .boundary_budget_for_live_owner(tid)
            .unwrap()
            .unwrap();
        assert_eq!(budget.expected().owner.tid, tid);
        assert_eq!(budget.stored_index(), 0);
        assert_eq!(
            scheduler.boundary_replayer.as_ref().unwrap().stored_index(),
            0
        );
        let new_mm = mm.for_exec(tid);
        scheduler
            .boundary_owners
            .as_mut()
            .unwrap()
            .exec(tid, tid, mm, new_mm)
            .unwrap();
        scheduler.record_thread_mm(tid, new_mm);
        assert_eq!(scheduler.boundary_budget_for_live_owner(tid).unwrap(), None);
        // Old format retains its exact old replayer and producer rather than
        // trying to synthesize absent callback identities.
        Recording::Legacy(PreemptionRecord::from_sched_events(vec![
            SchedEvent::branches(tid, 1),
        ]))
        .write(&path)
        .unwrap();
        let legacy = Scheduler::new(&Config {
            strict_schedule_boundaries: false,
            ..config
        });
        assert!(legacy.replayer.is_some());
        assert!(legacy.boundary_replayer.is_none());
        assert!(legacy.boundary_owners.is_none());
        assert!(
            !legacy
                .preemption_writer
                .as_ref()
                .unwrap()
                .records_slice_boundaries()
        );
        let fresh = Scheduler::new(&Config {
            sequentialize_threads: true,
            record_preemptions: true,
            strict_schedule_boundaries: true,
            ..Config::default()
        });
        let ordinary = Scheduler::new(&Config {
            record_preemptions: true,
            ..Config::default()
        });
        assert!(ordinary.boundary_owners.is_none());
        assert!(
            !ordinary
                .preemption_writer
                .as_ref()
                .unwrap()
                .records_slice_boundaries()
        );
        assert!(fresh.boundary_owners.is_some());
        assert!(
            fresh
                .preemption_writer
                .as_ref()
                .unwrap()
                .records_slice_boundaries()
        );
    }

    #[test]
    fn boundary_acceptance_refuses_before_cursor_writer_or_owner_publication() {
        for invalid in 0..6 {
            let Record::TimesliceEnd(boundary) = marker(3, 1) else {
                unreachable!()
            };
            let tid = boundary.identity.owner.tid;
            let mm = MmId::initial(tid);
            let mut scheduler = scheduler(vec![Record::TimesliceEnd(boundary.clone())]);
            let mut owners = crate::slice_boundary::lifecycle::Registry::default();
            owners.fresh(tid, mm).unwrap();
            scheduler.boundary_owners = Some(owners);
            scheduler.preemption_writer =
                Some(PreemptionWriter::new(None).with_slice_boundaries().unwrap());
            let mut bad = boundary.clone();
            match invalid {
                0 => bad.identity.owner.birth_tid = DetTid::from_raw(4),
                1 => bad.identity.slice = 2,
                2 => scheduler.recorded_event_count = u64::MAX,
                3 => scheduler.recorded_event_count = 1,
                4 => bad.identity.committed_rcbs = 1,
                5 => scheduler.preemption_writer = Some(PreemptionWriter::new(None)),
                _ => unreachable!(),
            }
            let count = scheduler.recorded_event_count;
            let writer_index = scheduler
                .preemption_writer
                .as_ref()
                .unwrap()
                .next_boundary_index();
            assert!(
                scheduler.accept_slice_boundary(tid, mm, &bad).is_err(),
                "invalid {invalid}"
            );
            assert_eq!(
                scheduler.boundary_replayer.as_ref().unwrap().stored_index(),
                0
            );
            assert_eq!(scheduler.recorded_event_count, count);
            assert_eq!(
                scheduler
                    .preemption_writer
                    .as_ref()
                    .unwrap()
                    .next_boundary_index(),
                writer_index
            );
            scheduler
                .boundary_owners
                .as_ref()
                .unwrap()
                .validate_boundary(tid, mm, &boundary)
                .unwrap();
        }
    }

    #[test]
    fn boundary_recording_event_selector_refuses_without_consuming_or_losing_index() {
        let Record::TimesliceEnd(boundary) = marker(3, 1) else {
            unreachable!()
        };
        let tid = boundary.identity.owner.tid;
        let mm = MmId::initial(tid);
        let mut scheduler = Scheduler::new(&Config::default());
        let mut owners = crate::slice_boundary::lifecycle::Registry::default();
        owners.fresh(tid, mm).unwrap();
        scheduler.boundary_owners = Some(owners);
        scheduler.preemption_writer =
            Some(PreemptionWriter::new(None).with_slice_boundaries().unwrap());
        scheduler.stacktrace_events = Some(
            vec![(0, Some(SchedEvent::branches(tid, 1)), None)]
                .into_iter()
                .peekable(),
        );
        assert!(scheduler.accept_slice_boundary(tid, mm, &boundary).is_err());
        assert_eq!(scheduler.recorded_event_count, 0);
        assert_eq!(
            scheduler
                .preemption_writer
                .as_ref()
                .unwrap()
                .next_boundary_index(),
            Some(0)
        );
        assert_eq!(
            scheduler
                .stacktrace_events
                .as_mut()
                .unwrap()
                .peek()
                .unwrap()
                .0,
            0
        );
        scheduler.stacktrace_events = None;
        assert!(
            scheduler
                .accept_slice_boundary(tid, mm, &boundary)
                .unwrap()
                .is_none()
        );
        assert_eq!(scheduler.recorded_event_count, 1);
        assert!(scheduler.accept_slice_boundary(tid, mm, &boundary).is_err());
        assert_eq!(
            scheduler
                .preemption_writer
                .as_ref()
                .unwrap()
                .next_boundary_index(),
            Some(1)
        );
    }

    #[test]
    fn same_owner_marker_does_not_requeue_or_advance_time_or_turn() {
        let first = marker(3, 1);
        let mut scheduler = scheduler(vec![first.clone(), marker(3, 2)]);
        for tid in [3, 4] {
            let tid = DetTid::from_raw(tid);
            scheduler.priorities.insert(tid, DEFAULT_PRIORITY);
            scheduler.runqueue_push_back(tid);
        }
        let queue = scheduler.run_queue.tids().copied().collect::<Vec<_>>();
        let before = (scheduler.turn, scheduler.committed_time);
        let result = scheduler.consume_boundary_record(&first).unwrap();
        assert!(result.keep_running);
        assert_eq!(
            scheduler.run_queue.tids().copied().collect::<Vec<_>>(),
            queue
        );
        assert_eq!((scheduler.turn, scheduler.committed_time), before);
        assert!(!scheduler.replay_priorities_released);
    }

    /// The replayed-SIGCHLD rule reads the boundary cursor too: a parked or
    /// arriving delivery is not held behind a replay-held polling retry when
    /// the next stored record, here a timeslice-end marker, is the signaled
    /// thread's, and it still is when the next record is the poller's
    /// (`Scheduler::sigchld_waits_behind_work`).
    #[test]
    fn a_replayed_sigchld_reads_the_boundary_cursor() {
        let (poller, signaled) = (DetTid::from_raw(3), DetTid::from_raw(7));
        for (next_owner, waits) in [(7, false), (3, true)] {
            let mut scheduler = scheduler(vec![marker(next_owner, 1)]);
            let mut polling = Resources::new(poller);
            polling.insert(ResourceID::InternalIOPolling, Permission::W);
            scheduler.next_turns.insert(
                poller,
                ThreadNextTurn {
                    dettid: poller,
                    child_tid_addr: 0,
                    req: Ivar::full(Ok(polling)),
                    resp: Ivar::new(),
                    protocol: Default::default(),
                },
            );
            scheduler
                .priorities
                .insert(poller, REPLAY_DEFERRED_PRIORITY);
            scheduler.runqueue_push_back(poller);
            assert!(scheduler.run_queue.has_runnable_besides(signaled));
            assert_eq!(
                scheduler.sigchld_waits_behind_work(signaled),
                waits,
                "next record owned by {next_owner}"
            );
            scheduler.blocked.sigchld_deferred.insert(signaled);
            assert_eq!(
                scheduler.replay_readmits_parked_sigchld(),
                (!waits).then_some(signaled),
                "next record owned by {next_owner}"
            );
        }
    }

    /// The boundary replayer switches away before the next thread's record at
    /// a syscall prehook and at a timeslice-end marker alike. Only the prehook
    /// leaves the thread a call to carry out (`replay_prehook_handoff`): the
    /// request after a marker resumes guest code, which records events, so it
    /// is not handed a turn ahead of the other deferred threads.
    #[test]
    fn only_a_prehook_switch_leaves_its_thread_a_call_to_carry_out() {
        let (current, next) = (DetTid::from_raw(3), DetTid::from_raw(4));
        let prehook = Record::Event(
            SchedEvent::syscall(current, Sysno::clone3, SyscallPhase::Prehook)
                .with_time(LogicalTime::from_nanos(10_000)),
        );
        let following = Record::Event(SchedEvent::branches(next, 10));
        for (observed, hands_over) in [(prehook, true), (marker(3, 1), false)] {
            let mut scheduler = scheduler(vec![observed.clone(), following.clone()]);
            for tid in [current, next] {
                scheduler.priorities.insert(tid, DEFAULT_PRIORITY);
                scheduler.runqueue_push_back(tid);
            }
            let result = scheduler.consume_boundary_record(&observed).unwrap();
            assert!(result.keep_running, "{observed:?}");
            assert_eq!(scheduler.priorities[&current], REPLAY_DEFERRED_PRIORITY);
            assert_eq!(scheduler.priorities[&next], REPLAY_FOREGROUND_PRIORITY);
            assert_eq!(
                scheduler.replay_prehook_callers.contains(&current),
                hands_over,
                "{observed:?}"
            );
        }
    }

    #[test]
    fn pending_marker_and_refused_branch_fragment_keep_readmission_deferred_until_exact_eof() {
        let branch = SchedEvent::branches(DetTid::from_raw(3), 10);
        let last = marker(3, 1);
        let mut scheduler = scheduler(vec![Record::Event(branch.clone()), last.clone()]);
        let peer = DetTid::from_raw(4);
        scheduler
            .priorities
            .insert(DetTid::from_raw(3), REPLAY_FOREGROUND_PRIORITY);
        scheduler.priorities.insert(peer, REPLAY_DEFERRED_PRIORITY);
        let mut prefix = branch.clone();
        prefix.count = 4;
        assert!(
            scheduler
                .boundary_replayer
                .as_mut()
                .unwrap()
                .observe(&Record::Event(prefix))
                .is_err()
        );
        assert_eq!(
            scheduler.boundary_replayer.as_ref().unwrap().stored_index(),
            0
        );
        scheduler.release_replay_priorities_if_exhausted();
        scheduler.readmit_ready_thread(peer);
        assert_eq!(
            scheduler.run_queue.queued_priority(peer),
            Some(REPLAY_DEFERRED_PRIORITY)
        );
        assert!(!scheduler.replay_priorities_released);
        assert!(scheduler.generate_partial_run_summary(None).is_err());
        scheduler.consume_schedevent(&branch);
        assert!(
            !scheduler.replay_priorities_released,
            "the final marker still owns the stream"
        );
        assert!(scheduler.generate_partial_run_summary(None).is_err());
        scheduler.consume_boundary_record(&last).unwrap();
        assert!(scheduler.replay_priorities_released);
        assert_eq!(scheduler.priorities[&peer], DEFAULT_PRIORITY);
        let queue = scheduler.run_queue.tids().copied().collect::<Vec<_>>();
        scheduler.release_replay_priorities_if_exhausted();
        assert_eq!(
            scheduler.run_queue.tids().copied().collect::<Vec<_>>(),
            queue
        );
        let late = DetTid::from_raw(7);
        assert!(!scheduler.replay_registration_priority(late, REPLAY_DEFERRED_PRIORITY));
        assert_eq!(scheduler.priorities[&late], DEFAULT_PRIORITY);
        assert!(scheduler.run_queue.remove_tid(peer));
        scheduler.readmit_ready_thread(peer);
        assert!(scheduler.run_queue.queued_priority(peer).unwrap() < REPLAY_FOREGROUND_PRIORITY);
        assert_eq!(
            scheduler
                .generate_partial_run_summary(None)
                .unwrap()
                .schedevent_replayed,
            2
        );
    }

    #[test]
    fn boundary_eof_changes_only_dispatch_not_logical_priority() {
        for exhaust in [false, true] {
            let tid = DetTid::from_raw(3);
            let peer = DetTid::from_raw(4);
            let branch = SchedEvent::branches(tid, 10);
            let last = marker(3, 1);
            let mut scheduler = scheduler(vec![Record::Event(branch.clone()), last.clone()]);
            scheduler.record_logical_replay_priorities = true;
            scheduler.preemption_writer =
                Some(PreemptionWriter::new(None).with_slice_boundaries().unwrap());
            for (thread, priority, vfork) in [
                (tid, REPLAY_FOREGROUND_PRIORITY, false),
                (peer, REPLAY_DEFERRED_PRIORITY, true),
            ] {
                scheduler.register_thread_for_test(thread);
                scheduler.priorities.insert(thread, priority);
                scheduler.register_preemption_thread(thread, priority, vfork);
            }
            scheduler.readmit_ready_thread(peer);
            let before = (scheduler.turn, scheduler.committed_time);
            scheduler.consume_schedevent(&branch);
            assert_eq!(
                scheduler.boundary_replayer.as_ref().unwrap().stored_index(),
                1
            );
            assert!(!scheduler.replay_priorities_released);
            assert_eq!(scheduler.priorities[&tid], REPLAY_FOREGROUND_PRIORITY);
            assert_eq!(scheduler.priorities[&peer], REPLAY_DEFERRED_PRIORITY);
            if exhaust {
                scheduler.consume_boundary_record(&last).unwrap();
                let late_vfork = DetTid::from_raw(7);
                scheduler.register_thread_for_test(late_vfork);
                assert!(
                    !scheduler.replay_registration_priority(late_vfork, REPLAY_DEFERRED_PRIORITY)
                );
                scheduler.register_preemption_thread(late_vfork, REPLAY_DEFERRED_PRIORITY, true);
                assert_eq!(scheduler.priorities[&late_vfork], DEFAULT_PRIORITY);
                assert_eq!(
                    scheduler
                        .preemption_writer
                        .as_ref()
                        .unwrap()
                        .current_priority(late_vfork),
                    DEFAULT_PRIORITY - 1
                );
            }
            assert_eq!(
                scheduler.boundary_replayer.as_ref().unwrap().stored_index(),
                if exhaust { 2 } else { 1 }
            );
            assert_eq!(scheduler.replay_has_records_left(), !exhaust);
            assert_eq!(scheduler.replay_priorities_released, exhaust);
            let controls = if exhaust {
                [DEFAULT_PRIORITY, DEFAULT_PRIORITY]
            } else {
                [REPLAY_FOREGROUND_PRIORITY, REPLAY_DEFERRED_PRIORITY]
            };
            assert_eq!(scheduler.priorities[&tid], controls[0]);
            assert_eq!(scheduler.run_queue.queued_priority(peer), Some(controls[1]));
            assert_eq!((scheduler.turn, scheduler.committed_time), before);
            let exported: serde_json::Value =
                serde_json::from_str(&scheduler.preemption_writer.take().unwrap().into_string())
                    .unwrap();
            let rows = &exported["slice_boundary_recording"]["preemptions"]["per_thread"];
            assert_eq!(rows["3"]["final_prio"], DEFAULT_PRIORITY);
            assert_eq!(rows["4"]["final_prio"], DEFAULT_PRIORITY - 1);
            if exhaust {
                assert_eq!(rows["7"]["final_prio"], DEFAULT_PRIORITY - 1);
            }
            for thread in ["3", "4"] {
                assert_eq!(rows[thread]["prio_changes"], serde_json::json!([]));
            }
        }
    }

    #[test]
    fn stack_requests_wait_for_the_complete_stored_index_including_markers() {
        let branch = SchedEvent::branches(DetTid::from_raw(3), 10);
        let last = marker(3, 1);
        let mut scheduler = scheduler(vec![Record::Event(branch.clone()), last.clone()]);
        scheduler.boundary_stacktrace_events.insert(0, None);
        scheduler
            .boundary_stacktrace_events
            .insert(1, Some("marker.stack".into()));
        let mut part = branch.clone();
        part.count = 4;
        assert!(
            scheduler
                .boundary_replayer
                .as_mut()
                .unwrap()
                .observe(&Record::Event(part))
                .is_err()
        );
        assert_eq!(
            scheduler.boundary_replayer.as_ref().unwrap().stored_index(),
            0
        );
        assert!(scheduler.boundary_stacktrace_events.contains_key(&0));
        assert_eq!(
            scheduler.consume_schedevent(&branch).print_stack,
            Some(None)
        );
        assert_eq!(
            scheduler
                .consume_boundary_record(&last)
                .unwrap()
                .print_stack,
            Some(Some("marker.stack".into()))
        );
        assert!(scheduler.boundary_stacktrace_events.is_empty());
    }

    #[test]
    fn new_protocol_flush_failure_is_not_only_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("not-created").join("recording.json");
        let mut scheduler = scheduler(vec![]);
        scheduler.preemption_writer = Some(
            PreemptionWriter::new(Some(path.clone()))
                .with_slice_boundaries()
                .unwrap(),
        );
        assert!(scheduler.generate_partial_run_summary(Some(&path)).is_err());
        assert!(!path.exists());
    }
}
