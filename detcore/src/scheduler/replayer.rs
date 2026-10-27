/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::ops::Add;

use detcore_model::collections::ReplayCursor;
use detcore_model::pid::DetTid;
use detcore_model::schedule::Op;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::SyscallPhase;
use detcore_model::time::LogicalTime;
use reverie::syscalls::Sysno;
use tracing::debug;
use tracing::info;
use tracing::trace;

const LOOKAHEAD_WINDOW: usize = 10;

/// How many observed events in a row may leave the replayed schedule's next
/// event unmatched before the replay stops following the schedule
/// (`Replayer::observe_event`). An unmatched polling retry is not counted,
/// and does not end the run of unmatched events either. A replay that resynchronizes matches within a
/// few events: fast-forward looks `LOOKAHEAD_WINDOW` events ahead, and an
/// inserted event is followed by the recorded one. A thousand is a long way
/// past both, and short of a stuck run: a `sched_yield` spinner makes four
/// events per turn, so the replay lets it go after 250 turns.
pub(super) const UNMATCHED_EVENTS_BEFORE_RELEASE: u64 = 1000;

/// Where a replay dropped the rest of its schedule
/// (`Replayer::release_schedule`), for the run summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Release {
    /// The observed event the release happened at, by its number among the
    /// run's observed events that are not polling retries: how many retries
    /// a blocked call makes can depend on host timing, so a number that
    /// counted them would not name the same event in every run.
    pub(super) event: u64,
    /// How many recorded events the release left unreplayed.
    pub(super) unreplayed: usize,
}

/// Replayer
#[derive(Debug)]
pub struct Replayer {
    /// A cursor that holds our place in the global total order of events being replayed.
    pub cursor: ReplayCursor<SchedEvent>,
    /// Keep track of how many events we have observed during replay.  The current value is the
    /// event number of the NEXT event to replay.
    pub traced_event_count: u64,

    /// The number of events we've popped from the recording.
    pub events_popped: u64,

    /// How many observed events in a row have left the schedule's next event
    /// unmatched (`UNMATCHED_EVENTS_BEFORE_RELEASE`).
    unmatched_run: u64,

    /// How many observed events were not polling retries: the numbering of
    /// `Release::event`.
    observed_non_polling: u64,

    /// Where the replay dropped the rest of the schedule, if it did
    /// (`Replayer::release_schedule`).
    release: Option<Release>,

    /// Desync events observed per thread.  Counts different kinds of desyncs.
    pub desync_counts: BTreeMap<DetTid, DesyncStats>,
    /// A cached copy of the same (immutable) field in Config.
    pub replay_exhausted_panic: bool,
    /// A cached copy of the same (immutable) field in Config.
    pub die_on_desync: bool,
    /// The syscall each thread has in flight, as its observed events name it:
    /// the number its prehook named, and the number its latest unsuccessful
    /// attempt named (a retry may probe with another call than the guest's).
    syscalls_in_flight: BTreeMap<DetTid, (Option<Sysno>, Option<Sysno>)>,
    /// The last retry turn `keep_retry_turn` kept: the thread, how many
    /// schedule events remained then, and how many turns in a row that thread
    /// has kept with that many remaining.
    kept_retry: Option<(DetTid, usize, u32)>,

    /// The recorded background starts not yet due, by how many events they
    /// follow (`crate::preemptions::BackgroundStart`): thread and call.
    background_starts: BTreeMap<u64, VecDeque<(DetTid, u64)>>,
    /// The recorded background starts due now, in order, until the scheduler
    /// takes them (`take_background_start`).
    pending_starts: VecDeque<(DetTid, u64)>,
    /// Whether the replayed record notes every background start
    /// (`crate::preemptions::PreemptionRecord::notes_background_starts`).
    notes_background_starts: bool,
    /// How many recorded events a fast-forward dropped unconsumed, so that
    /// `events_popped + skipped` is the replay's position in the schedule,
    /// which places the background starts.
    skipped: u64,
}

/// How many retry turns in a row a polling thread keeps
/// (`Replayer::keep_retry_turn`) while the replayed schedule does not
/// advance. Past this the retry backs off as a poller, as outside a replay,
/// so a replay that has drifted from its record cannot spin forever on a poll
/// that nothing in the run will satisfy.
pub(super) const REPLAY_KEPT_RETRY_LIMIT: u32 = 1000;

/// The count of desyncs for a particular thread.
#[derive(Debug, PartialEq, Eq, Default, Copy, Clone)]
pub struct DesyncStats {
    /// Not inclusive of hard desyncs.  You must add soft+hard together to get total desyncs.
    pub soft: u64,
    pub hard: u64,
    /// The very worst kind, where we need to context switch, but we don't have a match on the
    /// current event. These are a subset of the hard desyncs.
    pub at_context_switch: u64,
    /// How many times do we find a match again, after a mismatch, specifically new obserserved
    /// events that were not expected.
    pub resync_insertions: u64,
    /// How many times do we find a match again, after a mismatch, specifically expected events that
    /// were not observed.
    pub resync_deletions: u64,
    /// How many times did we fast-forward to resync.
    pub fast_forwards: u64,

    /// A bit of statefulness: true when the last event processed was a desync.
    pub last_was_desync: bool,
}

impl Add for DesyncStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        DesyncStats {
            soft: self.soft + rhs.soft,
            hard: self.hard + rhs.hard,
            at_context_switch: self.at_context_switch + rhs.at_context_switch,
            resync_insertions: self.resync_insertions + rhs.resync_insertions,
            resync_deletions: self.resync_deletions + rhs.resync_deletions,
            fast_forwards: self.fast_forwards + rhs.fast_forwards,

            // Not commutative!
            last_was_desync: rhs.last_was_desync,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    FatalDesync,
    ReplayExausted,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayAction {
    Continue(Option<LogicalTime>),
    Stop(StopReason),
    /// The boolean indicates if the context switch should be immediate (or wait till the current
    /// thread blocks itself and naturally yields control).
    ContextSwitch(bool, DetTid, Option<LogicalTime>),
}

/// Exit has no returning guest continuation to background. Its synthetic
/// posthook is emitted only after the scheduler commits the Exit request;
/// the existing exit grant, rather than an extra TraceReplay yield, hands off.
pub(super) fn is_terminal_posthook(event: &SchedEvent) -> bool {
    matches!(
        event.op,
        Op::Syscall(
            reverie::syscalls::Sysno::exit | reverie::syscalls::Sysno::exit_group,
            SyscallPhase::Posthook
        )
    )
}

fn compare_desync(observed: &SchedEvent, expected: &SchedEvent) -> String {
    if !is_desync(observed, expected) {
        "SYNCED".to_string()
    } else if observed.op != expected.op {
        "FULL-OP-DESYNC".to_string()
    } else {
        let mut msg = "DESYNC".to_string();
        if (expected.start_rip.is_some() && observed.start_rip != expected.start_rip)
            || (expected.end_rip.is_some() && observed.end_rip != expected.end_rip)
        {
            msg = "RIP-".to_owned() + &msg;
        }
        if observed.end_time != expected.end_time {
            msg = "TIME-".to_owned() + &msg;
        }
        if observed.op == Op::Branch
            && expected.op == Op::Branch
            && observed.count != expected.count
        {
            msg = "RCB-".to_owned() + &msg;
        }
        msg
    }
}

/// Do the events describe the same range of instructions, based on all the information present.
/// end_time is ignored because it depends on the global context, not just these events.
pub fn events_match(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    !is_desync(observed, expected)
}

fn is_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    if (observed.dettid, observed.op, observed.count)
        != (expected.dettid, expected.op, expected.count)
    {
        return true;
    }
    // It is ok for the observed event to contain information not present in the replay trace.
    if expected.end_time.is_some() && expected.end_time != observed.end_time {
        return true;
    }

    if expected.start_rip.is_some() && observed.start_rip != expected.start_rip {
        return true;
    }

    if expected.end_rip.is_some() && observed.end_rip != expected.end_rip {
        return true;
    }

    false
}

/// The fields that make `observed` a hard desync from `expected` (see
/// [`is_hard_desync`]), by name, so that a fatal desync says what differed.
/// A schedule recorded on KVM before KVM reported the guest's own return
/// address shows `end_rip` here: its syscall events hold the address of
/// the backend's trampoline.
fn hard_desync_fields(observed: &SchedEvent, expected: &SchedEvent) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if observed.dettid != expected.dettid {
        fields.push("dettid");
    }
    if observed.op != expected.op {
        fields.push("op");
    }
    // A RIP the trace leaves open matches any observed one, as in
    // `is_hard_desync`.
    if expected.start_rip.is_some() && observed.start_rip != expected.start_rip {
        fields.push("start_rip");
    }
    if expected.end_rip.is_some() && observed.end_rip != expected.end_rip {
        fields.push("end_rip");
    }
    fields
}

fn is_hard_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    let mut strip1 = observed.clone();
    let mut strip2 = expected.clone();
    strip1.end_time = None;
    strip2.end_time = None;
    strip1.count = 0;
    strip2.count = 0;
    // As in `is_desync`, the observed event may carry information the trace
    // lacks: a recorded timer preemption's branch event has no end RIP while the
    // replayed one does, and that is the same event, not a different one.
    if expected.start_rip.is_none() {
        strip1.start_rip = None;
    }
    if expected.end_rip.is_none() {
        strip1.end_rip = None;
    }
    strip1 != strip2
}

impl Replayer {
    /// Allocate a new replayer.
    pub fn new(i: impl IntoIterator<Item = SchedEvent>) -> Replayer {
        let cursor = ReplayCursor::from_iter(i);
        Replayer {
            syscalls_in_flight: BTreeMap::new(),
            kept_retry: None,
            cursor,
            desync_counts: BTreeMap::new(),
            die_on_desync: false,
            replay_exhausted_panic: false,
            traced_event_count: 0,
            events_popped: 0,
            unmatched_run: 0,
            observed_non_polling: 0,
            release: None,
            background_starts: BTreeMap::new(),
            pending_starts: VecDeque::new(),
            notes_background_starts: false,
            skipped: 0,
        }
    }

    /// The same replayer, with the background starts its record holds
    /// (`crate::preemptions::BackgroundStart`), each due once the events it
    /// follows are consumed. `notes_all` says whether the record notes every
    /// start its run took.
    pub fn with_background_starts(
        mut self,
        starts: &[crate::preemptions::BackgroundStart],
        notes_all: bool,
    ) -> Self {
        for start in starts {
            self.background_starts
                .entry(start.after)
                .or_default()
                .push_back((start.thread, start.call));
        }
        self.notes_background_starts = notes_all;
        self.queue_background_starts();
        self
    }

    /// A fast-forward dropped the next `dropped` recorded events. The starts
    /// recorded among them, or after the last of them (before the event the
    /// replay matched instead, which has already happened), are past: each
    /// counts a hard desync of its thread and is dropped, rather than coming
    /// due ahead of the start recorded after the matched event. The position
    /// moves past the dropped events, so later starts keep their places.
    /// Returns how many it retired.
    fn retire_skipped_starts(&mut self, dropped: u64) -> usize {
        let position = self.events_popped + self.skipped;
        let past: Vec<u64> = self
            .background_starts
            .range(position + 1..=position + dropped)
            .map(|(after, _)| *after)
            .collect();
        let mut retired = 0;
        for after in past {
            for (thread, call) in self.background_starts.remove(&after).unwrap_or_default() {
                tracing::warn!(
                    "[tracereplay] the recorded background start of thread {}'s call {} after event {} was fast-forwarded past",
                    thread,
                    call,
                    after
                );
                self.desync_counts.entry(thread).or_default().hard += 1;
                retired += 1;
            }
        }
        self.skipped += dropped;
        retired
    }

    /// Make the background starts recorded after the events the replay has
    /// passed due, in order. The position counts events a fast-forward
    /// dropped as passed (`retire_skipped_starts`).
    fn queue_background_starts(&mut self) {
        let position = self.events_popped + self.skipped;
        while let Some(entry) = self.background_starts.first_entry() {
            if *entry.key() > position {
                break;
            }
            self.pending_starts.extend(entry.remove());
        }
    }

    /// The thread and call of the background start the recording took next,
    /// if one is due: the schedule's next decision, before its next event.
    pub(super) fn pending_background_start(&self) -> Option<(DetTid, u64)> {
        self.pending_starts.front().copied()
    }

    /// Whether the replayed record notes every background start, so that a
    /// call it does not start at a point did not start there.
    pub(super) fn notes_background_starts(&self) -> bool {
        self.notes_background_starts
    }

    /// The scheduler began `thread`'s kernel-run call `call`. Whether that was
    /// the recorded background start due next, which it then retires.
    pub(super) fn take_background_start(&mut self, thread: DetTid, call: u64) -> bool {
        if self.pending_starts.front() == Some(&(thread, call)) {
            self.pending_starts.pop_front();
            true
        } else {
            false
        }
    }

    /// Whether the replay must stop the thread of `next`, a branch event, at
    /// its branch count with a PMU timer, and if so the count, relative to the
    /// thread's clock.
    ///
    /// A timer preemption records its branches alone, with the RIP the PMU
    /// stopped at: the stop comes right after a branch, so the recording gives
    /// it no OtherInstructions event (since 832ca40b7; before it, the stop had
    /// one). Only then does the replay need the timer, to stop the thread
    /// where the recording did, even when the thread is picked again and its
    /// next check-in follows after no further branch. The fragments
    /// split-branches and `hermit analyze` cut a branch event into carry no
    /// RIP, but are followed directly by more branches or by another thread's
    /// event, which marks them as stops too.
    ///
    /// Branches without a RIP followed by the thread's OtherInstructions
    /// reached a check-in after them: a syscall, a trapped instruction, a
    /// signal or an exec. The thread reaches that check-in on its own, and a
    /// context switch the schedule puts after it happens there. A stop on the
    /// branch count would land on the instructions between the last branch
    /// and the check-in and end the slice there, with a scheduler turn the
    /// recording does not have.
    fn resolve_time_to_run(&self, next: &SchedEvent) -> Option<LogicalTime> {
        let SchedEvent {
            op: Op::Branch,
            count,
            dettid,
            end_rip,
            ..
        } = next
        else {
            return None;
        };
        let timer_preemption = end_rip.is_some()
            || self
                .cursor
                .peek_nth(1)
                .is_some_and(|following| following.op == Op::Branch || following.dettid != *dettid);
        timer_preemption.then(|| {
            trace!("branch count to stop at before the next event: {}", count);
            LogicalTime::from_rcbs(u64::from(*count))
        })
    }

    fn pop_next(&mut self) -> Option<SchedEvent> {
        self.events_popped += 1;
        let next = self.cursor.next();
        self.queue_background_starts();
        next
    }

    /// The thread of the first remaining recorded event that one of
    /// `threads` runs, other than `except`, if any.
    pub(super) fn next_thread_among(&self, threads: &[DetTid], except: DetTid) -> Option<DetTid> {
        (0..)
            .map_while(|index| self.cursor.peek_nth(index))
            .map(|event| event.dettid)
            .find(|thread| *thread != except && threads.contains(thread))
    }

    fn peek_next(&self) -> Option<SchedEvent> {
        self.cursor.peek().cloned()
    }

    /// The next task and its relative duration, without consuming a replay
    /// event. A successful exec can replace that task's scheduler registration
    /// after its prehook already selected it as the replay foreground task.
    ///
    /// A recorded background start due now comes first: the recording took
    /// it before its next event, and its turn runs no guest code, so it has no
    /// slice length.
    pub(super) fn next_resume(&self) -> Option<(DetTid, Option<LogicalTime>)> {
        if let Some((thread, _)) = self.pending_background_start() {
            return Some((thread, None));
        }
        self.cursor
            .peek()
            .map(|next| (next.dettid, self.resolve_time_to_run(next)))
    }

    /// Whether the next replayed event is `tid` retrying a blocked syscall: either
    /// another unsuccessful attempt or its completion, of the syscall `tid`'s
    /// observed events say it has in flight. A recorded event of another
    /// syscall is not this retry.
    pub(super) fn next_is_retry_by(&self, tid: DetTid) -> bool {
        self.cursor.peek().is_some_and(|next| {
            next.dettid == tid
                && match next.op {
                    Op::Syscall(sysno, SyscallPhase::Polling | SyscallPhase::Posthook) => self
                        .syscalls_in_flight
                        .get(&tid)
                        .is_some_and(|(entered, attempted)| {
                            *entered == Some(sysno) || *attempted == Some(sysno)
                        }),
                    _ => false,
                }
        })
    }

    /// Count one more retry turn kept by `tid`, and say whether it may keep
    /// it: no more than [`REPLAY_KEPT_RETRY_LIMIT`] turns in a row while the
    /// replayed schedule does not advance. Every recorded event the replay
    /// consumes, or drops to resync, starts the count again.
    pub(super) fn keep_retry_turn(&mut self, tid: DetTid) -> bool {
        let remaining = self.cursor.len();
        let kept = match self.kept_retry {
            Some((thread, left, kept)) if thread == tid && left == remaining => {
                kept.saturating_add(1)
            }
            _ => 1,
        };
        self.kept_retry = Some((tid, remaining, kept));
        if kept == REPLAY_KEPT_RETRY_LIMIT + 1 {
            tracing::warn!(
                "[tracereplay] dtid {} kept {} retry turns in a row without the replayed schedule advancing; it backs off as a poller from now on. Next recorded event: {}",
                tid,
                REPLAY_KEPT_RETRY_LIMIT,
                self.cursor
                    .peek()
                    .map_or_else(|| "none".to_string(), ToString::to_string),
            );
        }
        kept <= REPLAY_KEPT_RETRY_LIMIT
    }

    /// Whether the next replayed event is `tid`'s unsuccessful attempt at a
    /// blocked syscall: the recording retried it and failed again.
    pub(super) fn next_is_failed_attempt_by(&self, tid: DetTid) -> bool {
        self.cursor.peek().is_some_and(|next| {
            next.dettid == tid && matches!(next.op, Op::Syscall(_, SyscallPhase::Polling))
        })
    }

    /// Whether the replay dropped the rest of the schedule after a long run of
    /// unmatched events (`Replayer::release_schedule`).
    pub(super) fn released(&self) -> bool {
        self.release.is_some()
    }

    /// Where the replay dropped the rest of the schedule, if it did.
    pub(super) fn release(&self) -> Option<&Release> {
        self.release.as_ref()
    }

    /// Whether the recorded schedule still has an event to replay, or a
    /// background start to take (after its last event, a start is still the
    /// schedule's). Past its end the run goes on unguided
    /// (`ReplayAction::Continue`).
    pub(super) fn has_events_left(&self) -> bool {
        self.cursor.peek().is_some()
            || !self.pending_starts.is_empty()
            || !self.background_starts.is_empty()
    }

    /// How many recorded background starts the replay has not taken, due or
    /// not yet due: at the end of a run, starts it left out.
    pub(super) fn unconsumed_background_starts(&self) -> usize {
        self.pending_starts.len()
            + self
                .background_starts
                .values()
                .map(VecDeque::len)
                .sum::<usize>()
    }

    /// The scheduler began `thread`'s call although no recorded start of it is
    /// due, in a replay of a record that notes every start, within the
    /// schedule: a hard desync of `thread`. Whether the replay must stop
    /// (`die_on_desync`).
    pub(super) fn note_unrecorded_background_start(&mut self, thread: DetTid) -> bool {
        self.desync_counts.entry(thread).or_default().hard += 1;
        self.die_on_desync
    }

    fn next_replay_matches(&self, observed: &SchedEvent) -> bool {
        if let Some(expected) = self.cursor.peek() {
            !is_hard_desync(observed, expected)
        } else {
            false
        }
    }

    pub fn observe_event(&mut self, observed: &SchedEvent) -> ReplayAction {
        let mytid = observed.dettid;
        let current_ix = self.traced_event_count;
        self.traced_event_count += 1;
        let polling = matches!(observed.op, Op::Syscall(_, SyscallPhase::Polling));
        let non_polling_ix = self.observed_non_polling;
        if !polling {
            self.observed_non_polling += 1;
        }
        self.note_syscall_in_flight(observed);
        // The recording began its due background starts before this event. A
        // start still due was not taken: the call the recording began here is
        // not waiting to begin, so the replay has left its recording. It
        // counts as a hard desync of the start's thread, and will not be taken.
        let untaken = std::mem::take(&mut self.pending_starts);
        for &(thread, call) in &untaken {
            tracing::warn!(
                "[tracereplay] the recorded background start of thread {}'s call {} before event #{} was not taken",
                thread,
                call,
                current_ix
            );
            self.desync_counts.entry(thread).or_default().hard += 1;
        }
        if !untaken.is_empty() && self.die_on_desync {
            return ReplayAction::Stop(StopReason::FatalDesync);
        }

        if let Some(expected) = self.peek_next() {
            debug!(
                "[detcore, dtid {}] {}: Ran event #{} {}, current replay event: {}",
                mytid,
                compare_desync(observed, &expected),
                current_ix,
                observed,
                expected,
            );
            if let Some(nxt) = self.cursor.peek_nth(1) {
                trace!("[detcore, dtid {}] NEXT event to replay {}", mytid, nxt);
            }
            if observed.dettid != expected.dettid {
                tracing::warn!(
                    "[tracereplay] expected to be running thread {}, but running {} at event #{}",
                    expected.dettid,
                    observed.dettid,
                    current_ix
                );
            }

            let is_desync = is_desync(observed, &expected);
            let mut in_hard_desync_mode = false;
            if is_desync {
                let counts = self.desync_counts.entry(mytid).or_default();
                if is_hard_desync(observed, &expected) {
                    if self.die_on_desync {
                        eprintln!(
                            "Replay mode desynchronized from trace, bailing out: event #{} differs \
                             in {} (observed {}, recorded {})",
                            current_ix,
                            hard_desync_fields(observed, &expected).join(", "),
                            observed,
                            expected,
                        );
                        return ReplayAction::Stop(StopReason::FatalDesync);
                    }
                    counts.hard += 1;
                    in_hard_desync_mode = true;
                } else {
                    // TODO: we need to handle a mismatch in counts by doing a partial-pop on the
                    // replayer state.
                    counts.soft += 1;
                }
            }
            self.counts_next_round(mytid, is_desync);

            if is_desync && let Some(ix) = self.check_fast_forward(observed) {
                debug_assert!(ix > 0);
                // This counts DELETIONS (missing events from observed stream):
                let counts = self.desync_counts.entry(mytid).or_default();
                counts.resync_deletions += 1;
                // The background starts recorded among the dropped events were
                // missed, as an untaken due start is, before the matched event
                // is accepted.
                if self.retire_skipped_starts(ix as u64) > 0 && self.die_on_desync {
                    return ReplayAction::Stop(StopReason::FatalDesync);
                }
                in_hard_desync_mode = false;
                // is_desync = false; // Logically correct, but... unused_assignments
            }

            if in_hard_desync_mode {
                // A polling retry does not count: how many retries a blocked
                // call makes can depend on host timing, when something outside
                // the container makes its descriptor ready
                // (`Scheduler::is_polling_turn`), and the count decides the
                // event the release happens at.
                if !polling {
                    self.unmatched_run += 1;
                    if self.unmatched_run >= UNMATCHED_EVENTS_BEFORE_RELEASE {
                        return self.release_schedule(non_polling_ix);
                    }
                }
            } else {
                // Whether naturally or by fast forward, we should end up in this state before we pop:
                if !self.next_replay_matches(observed) {
                    tracing::warn!(
                        "Expected match before pop, observed: {}\n Cursor at: {}",
                        observed,
                        self.cursor.display_first_n(10)
                    );
                }
                let _ = self.pop_next();
                self.unmatched_run = 0;
            }

            let action = self.check_context_switch(observed);
            if let ReplayAction::ContextSwitch(_, next_tid, _) = action
                && in_hard_desync_mode
            {
                tracing::warn!(
                    "[dtid {}] context switch to {}, but in a desynced state at this event (#{}) in --replay-schedule-from",
                    &mytid,
                    next_tid,
                    current_ix
                );
                let counts = self.desync_counts.entry(mytid).or_default();
                counts.at_context_switch += 1;
            }
            action
        } else if self.replay_exhausted_panic {
            eprintln!(
                "[detcore, dtid {}] Replay trace ran out, stopping at unknown event {:?}",
                mytid, observed
            );
            ReplayAction::Stop(StopReason::ReplayExausted)
        } else {
            info!(
                "[detcore, dtid {}] Replay trace ran out, unknown event {:?}",
                &mytid, observed
            );
            ReplayAction::Continue(None)
        }
    }

    /// Track which syscall `observed`'s thread has in flight
    /// (`next_is_retry_by`): a prehook enters one, an unsuccessful attempt
    /// names the call it retried with, and a posthook ends it.
    fn note_syscall_in_flight(&mut self, observed: &SchedEvent) {
        let tid = observed.dettid;
        match observed.op {
            Op::Syscall(sysno, SyscallPhase::Prehook) => {
                self.syscalls_in_flight.insert(tid, (Some(sysno), None));
            }
            Op::Syscall(sysno, SyscallPhase::Polling) => {
                self.syscalls_in_flight.entry(tid).or_default().1 = Some(sysno);
            }
            Op::Syscall(_, SyscallPhase::Posthook) => {
                self.syscalls_in_flight.remove(&tid);
            }
            _ => {}
        }
    }

    /// Stop following the schedule: `UNMATCHED_EVENTS_BEFORE_RELEASE`
    /// observed events in a row, through event `event` (numbered without
    /// polling retries, `Release::event`), matched none of its next event.
    ///
    /// The replay runs the thread the schedule names next and holds every
    /// other thread back. When that thread cannot produce the recorded event,
    /// a spinner waiting for a thread the replay holds back, say, the run
    /// stops making progress for good. Dropping the rest of the schedule
    /// ends the replay as running out of it does: the scheduler gives every
    /// thread the default priority (`Scheduler::consume_schedevent`), each
    /// thread's recorded callback checkpoints go with the events once it hears
    /// of the release (`Scheduler::schedule_replay_reply`), and the run goes
    /// on unguided. Under `--replay-exhausted-panic` the run stops here
    /// instead.
    ///
    /// The count is a liveness backstop, and a heuristic: a detour longer than
    /// the threshold that would have rejoined the schedule loses the rest of
    /// it too. It is a function of the observed events in turn order. An
    /// unmatched polling retry neither counts nor ends the run, since how many
    /// a blocked call makes can be host-timed. A retry that matches a recorded
    /// one ends the run, as every matched event does, and how many retries the
    /// replay matches can be host-timed too (`Scheduler::is_polling_turn`). So
    /// the release can come at another event, or not at all, in another run.
    fn release_schedule(&mut self, current_ix: u64) -> ReplayAction {
        let dropped = self.cursor.by_ref().count();
        self.release = Some(Release {
            event: current_ix,
            unreplayed: dropped,
        });
        tracing::warn!(
            "[tracereplay] {} observed events in a row matched none of the replayed schedule's next event, through event #{} (events are numbered, and unmatched ones counted, without polling retries); the replay no longer follows the schedule ({} recorded events left unreplayed)",
            UNMATCHED_EVENTS_BEFORE_RELEASE,
            current_ix,
            dropped
        );
        if self.replay_exhausted_panic {
            eprintln!(
                "[detcore] The replay left its schedule at event #{} (numbered without polling retries) with {} recorded events left; stopping (--replay-exhausted-panic)",
                current_ix, dropped
            );
            return ReplayAction::Stop(StopReason::ReplayExausted);
        }
        ReplayAction::Continue(None)
    }

    /// Logically advance the counting machinery to the next round.
    fn counts_next_round(&mut self, mytid: DetTid, is_desync: bool) {
        let counts = self.desync_counts.entry(mytid).or_default();
        if is_desync {
            // Set it for the next round:
            counts.last_was_desync = true;
        } else {
            if counts.last_was_desync {
                // This counts INSERTION events (new events in observed stream), because we
                // previously desynced and left unmatched events on the replay cursor, which are
                // now matched.
                counts.resync_insertions += 1;
            }
            counts.last_was_desync = false;
        }
    }

    /// Return how many to drop to have observed equal to the head of the replay tape,
    /// i.e. jump past missing or mismatched events on the replay tape.
    fn check_fast_forward(&mut self, observed: &SchedEvent) -> Option<usize> {
        let print_win = |sz: usize| -> String {
            let mut acc = String::new();
            for i in 0..sz {
                if let Some(ev) = self.cursor.peek_nth(i) {
                    acc.push_str(&format!(" {}", ev));
                }
            }
            acc
        };

        if let Some(ix) = self.cursor.prefix_contains(LOOKAHEAD_WINDOW, |expected| {
            events_match(observed, expected)
        }) {
            let counts = self.desync_counts.entry(observed.dettid).or_default();
            counts.fast_forwards += 1;
            info!(
                "FAST FORWARD by dropping {} events from replay to resync: {}",
                ix,
                print_win(ix)
            );
            // Drop the corresponding number of of missed/mismatched events so that the observed
            // matches the head of the replay tape (which is to be popped).
            self.cursor.drop(ix);
            Some(ix)
        } else {
            trace!(
                "No fast-forward possible for {} in window{}",
                observed,
                print_win(LOOKAHEAD_WINDOW)
            );
            None
        }
    }

    fn check_context_switch(&mut self, observed: &SchedEvent) -> ReplayAction {
        // We run after the pop of any matched action, so the next action in replay represents the
        // FUTURE not the PRESENT/PAST.
        //
        // A recorded background start due now is the schedule's next
        // decision, before its next event or after its last one: its thread
        // runs next only to begin its call (`take_background_start`), with no
        // slice length.
        let next = match self.pending_background_start() {
            Some((thread, _)) => Some((thread, None)),
            None => self
                .cursor
                .peek()
                .map(|next_ev| (next_ev.dettid, self.resolve_time_to_run(next_ev))),
        };
        if let Some((next_tid, timeslice)) = next {
            let mytid = observed.dettid;
            let time = observed.end_time.expect("timestamps required for now");
            let is_prehook = matches!(observed.op, Op::Syscall(_, SyscallPhase::Prehook));

            if next_tid != observed.dettid {
                if is_prehook {
                    info!(
                        "[detcore, dtid {}] CONTEXT SWITCH to {} after this syscall blocks.  Reprioritizing at time {}",
                        &mytid, next_tid, time
                    );
                } else {
                    info!(
                        "[detcore, dtid {}] CONTEXT SWITCH to {} after the last event retired.  Reprioritizing at time {}",
                        &mytid, next_tid, time
                    );
                }

                // The *downgrading* of the current thread will be handled by the caller if the
                // context switch is *now*.  If the last traced event on this thread is instead
                // a prehook, well we don't deschedule the current thread quite yet.  Rather, we
                // let the thread plow ahead, and actually block on the syscall, which will have
                // the effect of descheduling the therad anyway.  After that, the priorities
                // be set so as to make sure the correct thread (next_tid) runs.
                //
                // Note: for *natural* schedules that are recorded from hermit, the only reason
                // to switch away from the thread after the prehook is if it blocked. But a
                // synthetic schedule can choose to do this arbitrarily, and we need to assign
                // semantics to that on replay.  If we have threads A&B, and events "A_prehook,
                // B_*, A_posthook", then does the effect of A's syscall happen before or after
                // B's logic?
                // A committed exit also hands off naturally: there is no
                // returning continuation on which to request a background.
                ReplayAction::ContextSwitch(
                    !(is_prehook || is_terminal_posthook(observed)),
                    next_tid,
                    timeslice,
                )
            } else {
                // We're still running the next event.
                ReplayAction::Continue(timeslice)
            }
        } else {
            ReplayAction::Continue(None)
        }
    }
}

#[cfg(test)]
mod builder {
    use detcore_model::pid::DetTid;
    use detcore_model::schedule::InstructionPointer;
    use detcore_model::schedule::Op;
    use detcore_model::schedule::SchedEvent;
    use detcore_model::schedule::SyscallPhase;
    use detcore_model::time::DetTime;
    use reverie::syscalls::Sysno;

    pub struct SchedTestBuilder {
        time: DetTime,
    }

    impl SchedTestBuilder {
        pub fn syscall_pre(&mut self, sys_no: Sysno, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_rip: InstructionPointer::new(end_rip),
                end_time: Some(self.time.as_nanos()),
                op: Op::Syscall(sys_no, SyscallPhase::Prehook),
            }
        }

        pub fn syscall_post(&mut self, sys_no: Sysno, end_rip: usize, tid: DetTid) -> SchedEvent {
            self.time.add_syscall();
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_rip: InstructionPointer::new(end_rip),
                end_time: Some(self.time.as_nanos()),
                op: Op::Syscall(sys_no, SyscallPhase::Posthook),
            }
        }

        pub fn other(&mut self, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_time: Some(self.time.as_nanos()),
                op: Op::OtherInstructions,
                end_rip: InstructionPointer::new(end_rip),
            }
        }

        pub fn branch(&mut self, count: u32, tid: DetTid) -> SchedEvent {
            self.time.add_rcbs(count as u64);
            SchedEvent {
                count,
                dettid: tid,
                end_rip: None,
                end_time: Some(self.time.as_nanos()),
                op: Op::Branch,
                start_rip: None,
            }
        }

        pub fn new() -> Self {
            Self {
                time: Default::default(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use detcore_model::schedule::InstructionPointer;
    use pretty_assertions::assert_eq;
    use reverie::syscalls::Sysno;

    use super::*;

    const SOME_RIP: usize = 1234566;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    fn strip_times(vec: Vec<SchedEvent>) -> Vec<SchedEvent> {
        vec.iter()
            .map(|se| SchedEvent {
                end_time: Some(LogicalTime::from_nanos(0)),
                ..*se
            })
            .collect()
    }

    /// A replay lets go of its schedule once `UNMATCHED_EVENTS_BEFORE_RELEASE`
    /// observed events in a row match none of its next event, and not before:
    /// a matched event starts the count again.
    #[test]
    fn a_replay_releases_its_schedule_after_a_long_unmatched_run() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.other(SOME_RIP, tid(3));
        let spinner = b.other(SOME_RIP + 8, tid(5));
        let mut replayer = Replayer::new([recorded.clone(), recorded.clone(), recorded.clone()]);

        let unmatched = |replayer: &mut Replayer, count: u64| {
            for _ in 0..count {
                assert_eq!(
                    replayer.observe_event(&spinner),
                    ReplayAction::ContextSwitch(true, tid(3), None)
                );
            }
        };
        unmatched(&mut replayer, UNMATCHED_EVENTS_BEFORE_RELEASE - 1);
        assert_eq!(
            replayer.observe_event(&recorded),
            ReplayAction::Continue(None)
        );
        unmatched(&mut replayer, UNMATCHED_EVENTS_BEFORE_RELEASE - 1);
        assert!(replayer.has_events_left());
        assert!(!replayer.released());

        assert_eq!(
            replayer.observe_event(&spinner),
            ReplayAction::Continue(None)
        );
        assert!(!replayer.has_events_left());
        assert!(replayer.released());
        // The last of the second run, after the matched event.
        assert_eq!(
            replayer.release(),
            Some(&Release {
                event: 2 * UNMATCHED_EVENTS_BEFORE_RELEASE - 1,
                unreplayed: 2,
            })
        );
    }

    /// Unmatched polling retries do not count toward the release, nor end the
    /// run of unmatched events, nor number the event it happens at: how many
    /// a blocked call makes can depend on host timing. The release, its
    /// number included, is the same with any number of them interleaved,
    /// none included.
    #[test]
    fn unmatched_polling_retries_do_not_count_toward_a_release() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.other(SOME_RIP, tid(3));
        let spinner = b.other(SOME_RIP + 8, tid(5));
        let retry = SchedEvent {
            op: Op::Syscall(Sysno::read, SyscallPhase::Polling),
            ..spinner.clone()
        };
        let held = ReplayAction::ContextSwitch(true, tid(3), None);
        let release_after = |retries: u64| {
            let mut replayer =
                Replayer::new([recorded.clone(), recorded.clone(), recorded.clone()]);
            for _ in 1..UNMATCHED_EVENTS_BEFORE_RELEASE {
                assert_eq!(replayer.observe_event(&spinner), held);
            }
            for _ in 0..retries {
                assert_eq!(replayer.observe_event(&retry), held);
            }
            assert!(!replayer.released());
            assert_eq!(
                replayer.observe_event(&spinner),
                ReplayAction::Continue(None)
            );
            *replayer.release().unwrap()
        };
        let expected = Release {
            event: UNMATCHED_EVENTS_BEFORE_RELEASE - 1,
            unreplayed: 3,
        };
        assert_eq!(release_after(0), expected);
        assert_eq!(release_after(2 * UNMATCHED_EVENTS_BEFORE_RELEASE), expected);
    }

    /// A fatal desync names the fields that differed: an event that matches
    /// except for its `end_rip` (a schedule whose syscall events hold a
    /// different return address) names exactly `end_rip`.
    #[test]
    fn a_hard_desync_names_the_fields_that_differ() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.other(SOME_RIP, tid(3));
        let observed = SchedEvent {
            end_rip: std::num::NonZeroUsize::new(SOME_RIP + 2),
            ..recorded
        };
        assert!(is_hard_desync(&observed, &recorded));
        assert_eq!(hard_desync_fields(&observed, &recorded), vec!["end_rip"]);
        let other_thread = SchedEvent {
            dettid: tid(4),
            ..observed
        };
        assert_eq!(
            hard_desync_fields(&other_thread, &recorded),
            vec!["dettid", "end_rip"]
        );
        assert!(hard_desync_fields(&recorded, &recorded).is_empty());
    }

    /// The fields a fatal desync names agree with `is_hard_desync`: a RIP the
    /// trace leaves open is never named, since it cannot cause the bail, and
    /// an event pair names a field exactly when it is a hard desync.
    #[test]
    fn a_hard_desync_never_names_a_rip_the_trace_leaves_open() {
        let mut b = builder::SchedTestBuilder::new();
        let open = SchedEvent {
            start_rip: None,
            end_rip: None,
            ..b.syscall_pre(Sysno::read, SOME_RIP, tid(3))
        };
        let observed = SchedEvent {
            dettid: tid(4),
            start_rip: std::num::NonZeroUsize::new(SOME_RIP),
            end_rip: std::num::NonZeroUsize::new(SOME_RIP + 2),
            ..open
        };
        assert!(is_hard_desync(&observed, &open));
        assert_eq!(hard_desync_fields(&observed, &open), vec!["dettid"]);

        let rips = [
            None,
            std::num::NonZeroUsize::new(SOME_RIP),
            std::num::NonZeroUsize::new(SOME_RIP + 2),
        ];
        for tid_o in [tid(3), tid(4)] {
            for start_o in rips {
                for end_o in rips {
                    for start_e in rips {
                        for end_e in rips {
                            let observed = SchedEvent {
                                dettid: tid_o,
                                start_rip: start_o,
                                end_rip: end_o,
                                ..open
                            };
                            let expected = SchedEvent {
                                start_rip: start_e,
                                end_rip: end_e,
                                ..open
                            };
                            assert_eq!(
                                hard_desync_fields(&observed, &expected).is_empty(),
                                !is_hard_desync(&observed, &expected),
                                "observed {observed:?}, recorded {expected:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_observed_rip_the_trace_lacks_is_not_a_hard_desync() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = SchedEvent {
            end_rip: None,
            ..b.branch(12203, tid(59))
        };
        let replayed = SchedEvent {
            end_rip: Some(InstructionPointer::new(0x7fff_f7c9_6c68).unwrap()),
            end_time: Some(LogicalTime::from_nanos(150)),
            ..recorded.clone()
        };
        assert!(
            !is_hard_desync(&replayed, &recorded),
            "the trace leaves the RIP open"
        );
        assert!(is_desync(&replayed, &recorded), "the time still differs");

        let other_rip = SchedEvent {
            end_rip: Some(InstructionPointer::new(0x1000).unwrap()),
            ..recorded.clone()
        };
        let recorded_with_rip = SchedEvent {
            end_rip: Some(InstructionPointer::new(0x2000).unwrap()),
            ..recorded
        };
        assert!(
            is_hard_desync(&other_rip, &recorded_with_rip),
            "a RIP the trace names must match"
        );
    }

    #[test]
    fn a_blocked_syscall_retry_is_recognized_only_for_its_thread() {
        let mut b = builder::SchedTestBuilder::new();
        let pre = b.syscall_pre(Sysno::read, SOME_RIP, tid(3));
        let polling = SchedEvent {
            op: Op::Syscall(Sysno::read, SyscallPhase::Polling),
            ..b.syscall_pre(Sysno::read, SOME_RIP, tid(3))
        };
        let schedule = vec![
            pre,
            polling,
            b.syscall_post(Sysno::read, SOME_RIP, tid(3)),
            b.branch(5, tid(5)),
        ];
        let mut replayer = Replayer::new(schedule.clone());

        let _ = replayer.observe_event(&schedule[0]);
        assert!(
            replayer.next_is_retry_by(tid(3)),
            "thread 3 polls its read next"
        );
        assert!(
            !replayer.next_is_retry_by(tid(5)),
            "thread 5 has nothing to retry"
        );
        let _ = replayer.observe_event(&schedule[1]);
        assert!(
            replayer.next_is_retry_by(tid(3)),
            "thread 3 completes its read next"
        );
        let _ = replayer.observe_event(&schedule[2]);
        assert!(!replayer.next_is_retry_by(tid(3)), "the read is done");
        assert!(
            !replayer.next_is_retry_by(tid(5)),
            "a branch is not a retry"
        );
    }

    /// A recorded retry is this thread's retry only if it names the syscall
    /// the thread has in flight: the one its prehook entered, or the probe its
    /// latest unsuccessful attempt used.
    #[test]
    fn a_recorded_retry_of_another_syscall_is_not_this_retry() {
        let mut b = builder::SchedTestBuilder::new();
        let polling = |b: &mut builder::SchedTestBuilder, sysno| SchedEvent {
            op: Op::Syscall(sysno, SyscallPhase::Polling),
            ..b.syscall_pre(sysno, SOME_RIP, tid(3))
        };
        // The recording completes a different call than the one in flight.
        let schedule = vec![
            b.syscall_pre(Sysno::read, SOME_RIP, tid(3)),
            b.syscall_post(Sysno::recvfrom, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert!(
            !replayer.next_is_retry_by(tid(3)),
            "nothing is in flight before the prehook"
        );
        let _ = replayer.observe_event(&schedule[0]);
        assert!(
            !replayer.next_is_retry_by(tid(3)),
            "thread 3 is in a read, and the recording completes a recvfrom"
        );

        // A retry that probes with another call matches either number.
        let schedule = vec![
            b.syscall_pre(Sysno::pselect6, SOME_RIP, tid(3)),
            polling(&mut b, Sysno::poll),
            polling(&mut b, Sysno::poll),
            b.syscall_post(Sysno::pselect6, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        let _ = replayer.observe_event(&schedule[0]);
        assert!(
            !replayer.next_is_retry_by(tid(3)),
            "no attempt has used the probe yet"
        );
        let _ = replayer.observe_event(&schedule[1]);
        assert!(
            replayer.next_is_retry_by(tid(3)),
            "the recording retries with the probe the attempt used"
        );
        let _ = replayer.observe_event(&schedule[2]);
        assert!(
            replayer.next_is_retry_by(tid(3)),
            "the recording completes the call the prehook entered"
        );
    }

    /// A thread keeps at most `REPLAY_KEPT_RETRY_LIMIT` retry turns in a row
    /// while the schedule stands still; consuming a recorded event starts the
    /// count again.
    #[test]
    fn kept_retry_turns_are_capped_while_the_schedule_stands_still() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.syscall_pre(Sysno::recvfrom, SOME_RIP, tid(3)),
            b.syscall_post(Sysno::recvfrom, SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::write, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        let _ = replayer.observe_event(&schedule[0]);
        for turn in 1..=REPLAY_KEPT_RETRY_LIMIT {
            assert!(replayer.keep_retry_turn(tid(3)), "kept turn {turn}");
        }
        assert!(!replayer.keep_retry_turn(tid(3)), "the cap");
        assert!(!replayer.keep_retry_turn(tid(3)), "still capped");
        let _ = replayer.observe_event(&schedule[1]);
        assert!(
            replayer.keep_retry_turn(tid(3)),
            "the schedule advanced, so the count starts again"
        );
    }

    /// A recorded background start becomes due once the events it follows
    /// are consumed, and comes before the next event as the thread to run
    /// next, with no slice length. Taking it retires it; a different call does
    /// not. A start still due when the next event is observed was not taken:
    /// it is dropped and counts as a hard desync of its thread.
    #[test]
    fn a_recorded_background_start_is_the_next_decision_after_its_events() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.syscall_pre(Sysno::ioctl, SOME_RIP, tid(5)),
            b.syscall_pre(Sysno::getpid, SOME_RIP, tid(7)),
            b.syscall_post(Sysno::getpid, SOME_RIP, tid(7)),
        ];
        let start = |after, raw, call| crate::preemptions::BackgroundStart {
            after,
            thread: tid(raw),
            call,
        };
        let mut replayer = Replayer::new(schedule.clone())
            .with_background_starts(&[start(1, 5, 1), start(2, 5, 2)], true);
        assert!(replayer.notes_background_starts());
        assert_eq!(replayer.pending_background_start(), None);
        assert_eq!(replayer.next_resume().map(|(t, _)| t), Some(tid(5)));

        // The caller's prehook: its start is due, so it stays the thread to run.
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(None)
        );
        assert_eq!(replayer.pending_background_start(), Some((tid(5), 1)));
        assert_eq!(replayer.next_resume(), Some((tid(5), None)));
        assert!(!replayer.take_background_start(tid(5), 9));
        assert!(replayer.take_background_start(tid(5), 1));
        assert_eq!(replayer.next_resume().map(|(t, _)| t), Some(tid(7)));

        // The next start is due after the second event and is not taken
        // before the third: a hard desync of its thread.
        replayer.observe_event(&schedule[1]);
        assert_eq!(replayer.pending_background_start(), Some((tid(5), 2)));
        replayer.observe_event(&schedule[2]);
        assert_eq!(replayer.pending_background_start(), None);
        assert_eq!(replayer.desync_counts[&tid(5)].hard, 1);
    }

    /// Starts recorded after the same event are taken in their recorded order
    /// (for binder reads, the order the driver's waiting threads take work
    /// in). A start after the last event is still the schedule's: the replay
    /// keeps the schedule open for it, switches to its thread at the last
    /// event, and counts it among the unconsumed starts until it is taken.
    /// Under `--die-on-desync`, a start not taken before the next event stops
    /// the replay.
    #[test]
    fn recorded_background_starts_keep_their_order_through_the_last_event() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.syscall_pre(Sysno::ioctl, SOME_RIP, tid(5)),
            b.syscall_pre(Sysno::ioctl, SOME_RIP, tid(7)),
        ];
        let start = |after, raw, call| crate::preemptions::BackgroundStart {
            after,
            thread: tid(raw),
            call,
        };
        let mut replayer = Replayer::new(schedule.clone())
            .with_background_starts(&[start(1, 5, 1), start(1, 9, 4), start(2, 7, 1)], true);
        assert_eq!(replayer.unconsumed_background_starts(), 3);
        replayer.observe_event(&schedule[0]);
        assert!(!replayer.take_background_start(tid(9), 4));
        assert!(replayer.take_background_start(tid(5), 1));
        assert_eq!(replayer.next_resume(), Some((tid(9), None)));
        assert!(replayer.take_background_start(tid(9), 4));
        assert_eq!(replayer.next_resume().map(|(t, _)| t), Some(tid(7)));

        // The last event, then a start after it: switch to its thread (here
        // the same one, so the replay continues with it), schedule still open.
        assert_eq!(
            replayer.observe_event(&schedule[1]),
            ReplayAction::Continue(None)
        );
        assert!(replayer.has_events_left());
        assert_eq!(replayer.next_resume(), Some((tid(7), None)));
        assert_eq!(replayer.unconsumed_background_starts(), 1);
        assert!(replayer.take_background_start(tid(7), 1));
        assert!(!replayer.has_events_left());
        assert_eq!(replayer.unconsumed_background_starts(), 0);

        // A start after the last event of another thread is switched to.
        let mut other =
            Replayer::new(schedule[..1].to_vec()).with_background_starts(&[start(1, 9, 4)], true);
        assert_eq!(
            other.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(false, tid(9), None)
        );

        // Under --die-on-desync, an untaken start stops the replay.
        let mut strict =
            Replayer::new(schedule.clone()).with_background_starts(&[start(1, 5, 1)], true);
        strict.die_on_desync = true;
        strict.observe_event(&schedule[0]);
        assert_eq!(
            strict.observe_event(&schedule[1]),
            ReplayAction::Stop(StopReason::FatalDesync)
        );
        assert_eq!(strict.desync_counts[&tid(5)].hard, 1);

        // A call begun with no start of it due is a hard desync of its thread.
        let mut unrecorded =
            Replayer::new(schedule).with_background_starts(&[start(1, 5, 1)], true);
        unrecorded.die_on_desync = true;
        assert!(unrecorded.note_unrecorded_background_start(tid(7)));
        assert_eq!(unrecorded.desync_counts[&tid(7)].hard, 1);
    }

    /// A fast-forward past recorded events retires the starts recorded among
    /// them or before the matched event as hard desyncs of their threads, and
    /// leaves the start recorded after the matched event to come due in its
    /// place.
    #[test]
    fn a_fast_forward_retires_the_starts_it_skips() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::getpid, SOME_RIP, tid(3)),
        ];
        let start = |after, raw, call| crate::preemptions::BackgroundStart {
            after,
            thread: tid(raw),
            call,
        };
        let mut replayer = Replayer::new(schedule)
            .with_background_starts(&[start(1, 5, 1), start(2, 7, 1), start(3, 9, 1)], true);
        // Drop the first two events: the starts after events 1 and 2 are past.
        replayer.retire_skipped_starts(2);
        assert_eq!(replayer.desync_counts[&tid(5)].hard, 1);
        assert_eq!(replayer.desync_counts[&tid(7)].hard, 1);
        assert_eq!(replayer.unconsumed_background_starts(), 1);
        // The matched event (the third) is consumed: the start after it is due.
        replayer.pop_next();
        assert_eq!(replayer.pending_background_start(), Some((tid(9), 1)));

        // Through `observe_event`: a branch event whose count differs from the
        // recorded one (a soft desync) fast-forwards to the matching event
        // after it, past a start recorded between them, which is missed, and
        // fatal under --die-on-desync, before the matched event is accepted.
        let mut b = builder::SchedTestBuilder::new();
        let schedule = strip_times(vec![
            b.branch(100, tid(3)),
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::getpid, SOME_RIP, tid(3)),
        ]);
        let observed = schedule[1].clone();
        let mut lenient =
            Replayer::new(schedule.clone()).with_background_starts(&[start(1, 5, 1)], true);
        assert_ne!(
            lenient.observe_event(&observed),
            ReplayAction::ContextSwitch(false, tid(5), None)
        );
        assert_eq!(lenient.desync_counts[&tid(5)].hard, 1);
        assert_eq!(lenient.pending_background_start(), None);
        let mut strict = Replayer::new(schedule).with_background_starts(&[start(1, 5, 1)], true);
        strict.die_on_desync = true;
        assert_eq!(
            strict.observe_event(&observed),
            ReplayAction::Stop(StopReason::FatalDesync)
        );
        assert_eq!(strict.desync_counts[&tid(5)].hard, 1);

        // The rest of the schedule survives the fast-forward: five events of
        // one thread with a start recorded after the fourth, and the second
        // observed first. The replay drops the first event, follows the rest,
        // takes the start when it comes due, and then is exhausted.
        let mut b = builder::SchedTestBuilder::new();
        let five = strip_times(vec![
            b.branch(10, tid(3)),
            b.branch(20, tid(3)),
            b.branch(30, tid(3)),
            b.branch(40, tid(3)),
            b.branch(50, tid(3)),
        ]);
        let mut resynced =
            Replayer::new(five.clone()).with_background_starts(&[start(4, 3, 1)], true);
        resynced.observe_event(&five[1]);
        assert_eq!(resynced.desync_counts[&tid(3)].fast_forwards, 1);
        assert_eq!(resynced.cursor.peek(), Some(&five[2]));
        resynced.observe_event(&five[2]);
        resynced.observe_event(&five[3]);
        assert_eq!(resynced.pending_background_start(), Some((tid(3), 1)));
        assert!(resynced.take_background_start(tid(3), 1));
        resynced.observe_event(&five[4]);
        assert_eq!(resynced.desync_counts[&tid(3)].hard, 0);
        assert!(!resynced.has_events_left());
        assert_eq!(resynced.unconsumed_background_starts(), 0);
    }

    #[test]
    fn test_continue_() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![b.branch(10, tid(3))];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(None)
        );
    }

    /// A timer preemption the thread runs on from (its branches followed by
    /// more of its branches) is replayed with a stop on the branch count.
    #[test]
    fn test_respect_branch_count() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)), //current instruction
            b.branch(123, tid(3)),     // next to respect branches
            b.branch(1, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    /// A timer preemption that switched threads (its branches followed by
    /// another thread's event) is replayed with a stop on the branch count.
    #[test]
    fn test_respect_branch_context_switch() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)), //current instruction
            b.branch(123, tid(3)),     // next to respect branches
            b.syscall_post(Sysno::exit, SOME_RIP, tid(5)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    /// A timer stop the recording gave its RIP keeps its stop when the thread
    /// was picked again and reached its next check-in after no further
    /// branch: its OtherInstructions follows directly.
    #[test]
    fn a_timer_stop_with_its_rip_keeps_its_stop_before_the_threads_check_in() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)),
            SchedEvent {
                end_rip: detcore_model::schedule::InstructionPointer::new(SOME_RIP),
                ..b.branch(123, tid(3))
            },
            b.other(SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::mmap, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(LogicalTime::from_rcbs(123)))
        );
    }

    /// Branches the recording followed with the thread's OtherInstructions
    /// reached a check-in, which the replayed thread reaches on its own: no
    /// stop on their count, whether the schedule switches threads after the
    /// check-in or the thread runs on.
    #[test]
    fn branches_before_a_recorded_check_in_get_no_stop() {
        for switches in [true, false] {
            let mut b = builder::SchedTestBuilder::new();
            let mut schedule = vec![
                b.other(SOME_RIP, tid(3)),
                b.branch(123, tid(3)),
                b.other(SOME_RIP, tid(3)),
            ];
            schedule.push(if switches {
                b.syscall_post(Sysno::exit, SOME_RIP, tid(5))
            } else {
                b.branch(1, tid(3))
            });
            let mut replayer = Replayer::new(schedule.clone());
            assert_eq!(
                replayer.observe_event(&schedule[0]),
                ReplayAction::Continue(None)
            );
        }
    }

    #[test]
    fn test_context_switch_pre_hook() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(5)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(true, tid(3), None)
        );
    }

    #[test]
    fn test_context_switch_post_hook() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(5)),
            b.syscall_post(Sysno::exit, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::ContextSwitch(true, tid(3), None)
        );
    }

    #[test]
    fn terminal_posthook_handoff_preserves_target_duration_and_cursor() {
        for syscall in [Sysno::exit, Sysno::exit_group] {
            let mut b = builder::SchedTestBuilder::new();
            let exited = b.syscall_post(syscall, SOME_RIP, tid(5));
            let next = SchedEvent {
                end_rip: detcore_model::schedule::InstructionPointer::new(SOME_RIP),
                ..b.branch(123, tid(3))
            };
            let mut r = Replayer::new(vec![exited.clone(), next.clone()]);
            r.die_on_desync = true;
            assert_eq!(
                r.observe_event(&exited),
                ReplayAction::ContextSwitch(false, tid(3), Some(LogicalTime::from_rcbs(123)))
            );
            assert_eq!(r.cursor.peek(), Some(&next));
            assert_eq!((r.traced_event_count, r.events_popped), (1, 1));
            assert_eq!(
                r.desync_counts,
                BTreeMap::from([(tid(5), DesyncStats::default())])
            );
            assert_eq!(r.observe_event(&next), ReplayAction::Continue(None));
            assert!(!r.has_events_left());
        }
    }

    #[test]
    fn returning_posthook_handoff_stays_immediate() {
        for syscall in [Sysno::getpid, Sysno::mprotect] {
            let mut b = builder::SchedTestBuilder::new();
            let returned = b.syscall_post(syscall, SOME_RIP, tid(5));
            let next = b.other(SOME_RIP, tid(3));
            let mut r = Replayer::new(vec![returned.clone(), next.clone()]);
            assert_eq!(
                r.observe_event(&returned),
                ReplayAction::ContextSwitch(true, tid(3), None)
            );
            assert_eq!(r.cursor.peek(), Some(&next));
            assert_eq!(
                r.desync_counts,
                BTreeMap::from([(tid(5), DesyncStats::default())])
            );
        }
    }

    #[test]
    fn terminal_posthook_mismatch_remains_fatal_without_consumption() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.syscall_post(Sysno::exit, SOME_RIP, tid(5));
        let observed = SchedEvent {
            op: Op::Syscall(Sysno::exit_group, SyscallPhase::Posthook),
            ..recorded
        };
        let mut r = Replayer::new(vec![recorded.clone(), b.other(SOME_RIP, tid(3))]);
        r.die_on_desync = true;
        assert_eq!(
            r.observe_event(&observed),
            ReplayAction::Stop(StopReason::FatalDesync)
        );
        assert_eq!(r.cursor.peek(), Some(&recorded));
        assert_eq!(r.events_popped, 0);
    }

    #[test]
    fn test_fast_forward() {
        let mut b = builder::SchedTestBuilder::new();

        // Strip times so we don't have a time mismatch in the branch 123 events:
        let observed = strip_times(vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ]);
        let expected = strip_times(vec![
            b.branch(123, tid(3)),
            b.syscall_pre(Sysno::exit, SOME_RIP, tid(3)),
        ]);
        let mut replayer = Replayer::new(expected);
        let action1 = replayer.observe_event(&observed[0]);
        {
            let counts = replayer.desync_counts.get(&tid(3)).unwrap();
            assert_eq!(
                *counts,
                DesyncStats {
                    soft: 0,
                    hard: 1,
                    at_context_switch: 0,
                    resync_insertions: 0,
                    resync_deletions: 0,
                    fast_forwards: 0,
                    last_was_desync: true,
                }
            );
        }
        assert_eq!(action1, ReplayAction::Continue(None));

        let action2 = replayer.observe_event(&observed[1]);
        {
            let counts = replayer.desync_counts.get(&tid(3)).unwrap();
            assert_eq!(
                *counts,
                DesyncStats {
                    soft: 0,
                    hard: 1,
                    at_context_switch: 0,
                    resync_insertions: 1,
                    resync_deletions: 0,
                    fast_forwards: 0,
                    last_was_desync: false,
                }
            );
        }
        assert_eq!(action2, ReplayAction::Continue(None));
    }
}
