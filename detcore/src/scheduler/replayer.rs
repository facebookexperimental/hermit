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
use detcore_model::summary::ReplayPosition;
use detcore_model::time::LogicalTime;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;
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

    /// How many events the replayed schedule has: the first position past
    /// its end (`stacktrace_position`).
    schedule_len: u64,
    /// The schedule position of the recorded event the last observed event
    /// consumed, if it consumed one (`stacktrace_position`).
    last_consumed: Option<u64>,
    /// The traced index of the first event observed after the replay consumed
    /// the schedule's last event, if it did (`stacktrace_position`). A release
    /// does not set it: the positions it drops are never consumed.
    exhausted_from: Option<u64>,

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
    /// The first fatal desync, for the ERROR line at the stop:
    /// (thread, event index, recorded, observed).
    pub fatal_desync: Option<(DetTid, u64, String, String)>,
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
    /// takes them (`take_background_start`): thread, call, and the `after`
    /// that placed them.
    pending_starts: VecDeque<(DetTid, u64, u64)>,
    /// The recorded background starts the replay passed without taking them,
    /// with where it was then (`untaken_background_starts`).
    passed_starts: Vec<UntakenStart>,
    /// Whether the replayed record notes every background start
    /// (`crate::preemptions::PreemptionRecord::notes_background_starts`).
    notes_background_starts: bool,
    /// How many recorded events a fast-forward dropped unconsumed, so that
    /// `events_popped + skipped` is the replay's position in the schedule
    /// (`record_position`), which places the background starts and the
    /// summary's desyncs.
    skipped: u64,
    /// The first replayed event that differed from the record, for the run
    /// summary (<https://github.com/rrnewton/hermit/issues/4876>).
    pub first_desync: Option<FirstDesync>,
}

/// How many retry turns in a row a polling thread keeps
/// (`Replayer::keep_retry_turn`) while the replayed schedule does not
/// advance. Past this the retry backs off as a poller, as outside a replay,
/// so a replay that has drifted from its record cannot spin forever on a poll
/// that nothing in the run will satisfy.
pub(super) const REPLAY_KEPT_RETRY_LIMIT: u32 = 1000;

/// A recorded background start the replay did not take
/// (`Replayer::untaken_background_starts`).
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct UntakenStart {
    /// The thread whose call it began.
    pub thread: DetTid,
    /// The call's per-thread sequence number.
    pub call: u64,
    /// How many recorded events came before it.
    pub after: u64,
    /// Where the replay was when it passed the start, or `None` if it never
    /// reached it.
    pub passed_at: Option<ReplayPosition>,
}

/// The first replayed event that differed from the record
/// (`Replayer::first_desync`).
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub struct FirstDesync {
    /// Its number, as `traced_event_count` and the `[tracereplay]` warnings
    /// number events.
    pub event: u64,
    /// The record's position then ([`Replayer::record_position`]): the index
    /// of the recorded event it stood for.
    pub record_event: u64,
    /// The thread that ran it.
    pub observed: DetTid,
    /// The thread of the recorded event.
    pub expected: DetTid,
    /// Whether it was a hard desync (`is_hard_desync`).
    pub hard: bool,
}

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

/// Where a replayed thread's timeslice ends: the branches to its next
/// recorded timeslice end (`Replayer::resolve_time_to_run`), as the logical
/// time they take, and whether the recording took that end without a stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayedSliceEnd {
    /// The branches to the end, relative to the thread's clock.
    pub length: LogicalTime,
    /// The recorded end is a lone thread's, taken without a stop
    /// (`SchedEvent::stopless`). The replay may take it the same way where
    /// its thread runs alone; otherwise it stops there.
    pub stopless: bool,
}

impl ReplayedSliceEnd {
    /// The same end, taken with a stop.
    fn with_a_stop(self) -> Self {
        Self {
            stopless: false,
            ..self
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReplayAction {
    Continue(Option<ReplayedSliceEnd>),
    Stop(StopReason),
    /// The boolean indicates if the context switch should be immediate (or wait till the current
    /// thread blocks itself and naturally yields control).
    ContextSwitch(bool, DetTid, Option<ReplayedSliceEnd>),
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
        if observed.instruction_offset != expected.instruction_offset {
            msg = "OFFSET-".to_owned() + &msg;
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

    // Where a timer armed past its branch target stopped the thread: the
    // replay arms the recorded one, so the observed event carries it exactly
    // when the expected one does.
    if observed.instruction_offset != expected.instruction_offset {
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
    if expected.end_rip.is_some() && observed.end_rip != expected.end_rip && !expected.stopless {
        fields.push("end_rip");
    }
    if observed.instruction_offset != expected.instruction_offset {
        fields.push("instruction_offset");
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
    // How a run took a slice end is not part of what the guest did. A
    // replay that stopped at an end its recording took without a stop has a
    // RIP the recording could not have.
    if expected.stopless {
        strip1.end_rip = None;
    }
    strip1.stopless = false;
    strip2.stopless = false;
    strip1 != strip2
}

impl Replayer {
    /// Allocate a new replayer.
    pub fn new(i: impl IntoIterator<Item = SchedEvent>) -> Replayer {
        let cursor = ReplayCursor::from_iter(i);
        let schedule_len = cursor.len() as u64;
        Replayer {
            syscalls_in_flight: BTreeMap::new(),
            kept_retry: None,
            cursor,
            desync_counts: BTreeMap::new(),
            die_on_desync: true,
            fatal_desync: None,
            replay_exhausted_panic: false,
            traced_event_count: 0,
            events_popped: 0,
            schedule_len,
            last_consumed: None,
            exhausted_from: None,
            unmatched_run: 0,
            observed_non_polling: 0,
            release: None,
            background_starts: BTreeMap::new(),
            pending_starts: VecDeque::new(),
            passed_starts: Vec::new(),
            notes_background_starts: false,
            skipped: 0,
            first_desync: None,
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
    /// `event` is the number of the observed event that caused the
    /// fast-forward, where the replay passed them. Returns how many it
    /// retired.
    fn retire_skipped_starts(&mut self, dropped: u64, event: u64) -> usize {
        let position = self.record_position();
        let past: Vec<u64> = self
            .background_starts
            .range(position + 1..=position + dropped)
            .map(|(after, _)| *after)
            .collect();
        let mut retired = 0;
        let passed_at = ReplayPosition {
            event,
            record_event: position,
        };
        for after in past {
            for (thread, call) in self.background_starts.remove(&after).unwrap_or_default() {
                tracing::warn!(
                    "[tracereplay] the recorded background start of thread {}'s call {} after event {} was fast-forwarded past",
                    thread,
                    call,
                    after
                );
                self.desync_counts.entry(thread).or_default().hard += 1;
                self.passed_starts.push(UntakenStart {
                    thread,
                    call,
                    after,
                    passed_at: Some(passed_at),
                });
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
        let position = self.record_position();
        while let Some(entry) = self.background_starts.first_entry() {
            if *entry.key() > position {
                break;
            }
            let after = *entry.key();
            self.pending_starts.extend(
                entry
                    .remove()
                    .into_iter()
                    .map(|(thread, call)| (thread, call, after)),
            );
        }
    }

    /// The thread and call of the background start the recording took next,
    /// if one is due: the schedule's next decision, before its next event.
    /// None once the replay has released its schedule (`release_schedule`):
    /// it no longer follows the record, so no recorded start is due, and the
    /// starts it left stay in the record for `untaken_background_starts`.
    pub(super) fn pending_background_start(&self) -> Option<(DetTid, u64)> {
        if self.released() {
            return None;
        }
        self.pending_starts
            .front()
            .map(|&(thread, call, _)| (thread, call))
    }

    /// Whether the replayed record notes every background start, so that a
    /// call it does not start at a point did not start there.
    pub(super) fn notes_background_starts(&self) -> bool {
        self.notes_background_starts
    }

    /// The scheduler began `thread`'s kernel-run call `call`. Whether that was
    /// the recorded background start due next, which it then retires.
    pub(super) fn take_background_start(&mut self, thread: DetTid, call: u64) -> bool {
        if self.pending_background_start() == Some((thread, call)) {
            self.pending_starts.pop_front();
            true
        } else {
            false
        }
    }

    /// Where the replay is in the record: the index, among the record's
    /// schedule events, of the next one to match. It equals
    /// `traced_event_count` until the first desync; after it, an event
    /// observed in hard desync consumes no recorded event and a fast-forward
    /// drops recorded events unobserved, so the two can part.
    pub fn record_position(&self) -> u64 {
        self.events_popped + self.skipped
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
    /// next check-in follows after no further branch. When split-branches or
    /// `hermit analyze` cuts a branch event into fragments, the leading ones
    /// carry no RIP but are followed directly by more branches or by another
    /// thread's event, which marks them as stops too, and the trailing one
    /// keeps the original's RIP, so it is a stop exactly where the original
    /// was.
    ///
    /// Branches without a RIP followed by the thread's OtherInstructions
    /// reached a check-in after them: a syscall, a trapped instruction, a
    /// signal or an exec. The thread reaches that check-in on its own, and a
    /// context switch the schedule puts after it happens there. A stop on the
    /// branch count would land on the instructions between the last branch
    /// and the check-in and end the slice there, with a scheduler turn the
    /// recording does not have.
    ///
    /// Branches a lone thread ran to a slice end taken without a stop
    /// (`SchedEvent::stopless`) always end a timeslice, whatever follows: the
    /// thread's own check-in may follow with no branch between, which the
    /// rule above would read as branches that reach a check-in.
    ///
    /// This legacy schedule replay re-records below
    /// `STOPLESS_SLICE_ENDS_FORMAT`: a following same-thread non-Branch would
    /// hide that end in the re-record once its mark is dropped, so the replay
    /// takes it with a stop instead (`Detcore::catch_up_lone_slice_ends`
    /// honors the same refusal for an end a skipped run already passed). A
    /// following Branch, another thread's event, or the record's own end
    /// keeps the slice end visible without the mark, so those ends may still
    /// be taken without a stop.
    fn resolve_time_to_run(&self, next: &SchedEvent) -> Option<ReplayedSliceEnd> {
        let branches = if next.stopless {
            Some(next.count)
        } else {
            self.timer_preemption_branches(next)
        };
        branches.map(|count| {
            let mut end = ReplayedSliceEnd {
                length: LogicalTime::from_rcbs(u64::from(count)),
                stopless: next.stopless,
            };
            if next.stopless {
                let end_stays_visible = self.cursor.peek_nth(1).is_none_or(|following| {
                    following.op == Op::Branch || following.dettid != next.dettid
                });
                if !end_stays_visible {
                    end = end.with_a_stop();
                }
            }
            end
        })
    }

    /// The branch count a timer preemption the recording took with a stop
    /// ends at, when `next` is one.
    fn timer_preemption_branches(&self, next: &SchedEvent) -> Option<u32> {
        let SchedEvent {
            op: Op::Branch,
            count,
            dettid,
            end_rip,
            instruction_offset,
            ..
        } = next
        else {
            return None;
        };
        // A batch with an instruction offset ended at an offset timer
        // (`--chaos-instruction-offset`) by construction. Its recording
        // follows it with the thread's OtherInstructions, the instructions
        // the timer stepped past the branch target, and gives it no RIP, so
        // the shape alone would read as a slice that reached a check-in.
        let timer_preemption = end_rip.is_some()
            || instruction_offset.is_some()
            || self
                .cursor
                .peek_nth(1)
                .is_some_and(|following| following.op == Op::Branch || following.dettid != *dettid);
        timer_preemption.then(|| {
            trace!("branch count to stop at before the next event: {}", count);
            *count
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
    /// slice length. Once the replay has released its schedule there is
    /// neither: the release drained the cursor, and no start is due
    /// (`pending_background_start`).
    pub(super) fn next_resume(&self) -> Option<(DetTid, Option<LogicalTime>)> {
        self.next_slice_end()
            .map(|(thread, end)| (thread, end.map(|end| end.length)))
    }

    /// Where the timer of the slice that [`Self::next_resume`]'s, and the last
    /// `ReplayAction`'s, timeslice runs to was armed past its branch target,
    /// if its recording armed it so (`SchedEvent::instruction_offset`). Read
    /// right after `observe_event`, it belongs to that action's timeslice:
    /// both come from the next event to replay, the timeslice from its
    /// branch count (`resolve_time_to_run`) and this from its offset, so an
    /// offset comes only with a timeslice.
    pub(super) fn next_instruction_offset(
        &self,
    ) -> Option<detcore_model::schedule::InstructionOffset> {
        let next = self.cursor.peek()?;
        self.resolve_time_to_run(next)?;
        next.instruction_offset
    }

    /// [`Self::next_resume`] with the replayed slice end itself: its length,
    /// and whether the recording took it without a stop.
    fn next_slice_end(&self) -> Option<(DetTid, Option<ReplayedSliceEnd>)> {
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

    /// The schedule position of the event just observed, at traced index
    /// `traced_ix`, for `--stacktrace-at-schedule-positions`: the position of
    /// the recorded event it consumed, or, once the replay has consumed the
    /// schedule's last event, a position past the end (the first event after
    /// it is position `schedule_len`). `None` for an event the replay did not
    /// consume, and for every event after a release.
    ///
    /// Positions follow the recording, not the observed count: an event the
    /// schedule does not hold (a polling retry the recording did not make,
    /// say) neither takes a position nor shifts the later ones, and a recorded
    /// event the replay skips to resync takes no observed event.
    pub(super) fn stacktrace_position(&self, traced_ix: u64) -> Option<u64> {
        if let Some(position) = self.last_consumed {
            return Some(position);
        }
        self.exhausted_from
            .map(|first| self.schedule_len + (traced_ix - first))
    }

    /// Whether the recorded schedule still has an event to replay, or a
    /// background start to take (after its last event, a start is still the
    /// schedule's). Past its end the run goes on unguided
    /// (`ReplayAction::Continue`).
    ///
    /// A released schedule has neither (`release_schedule`). The release
    /// drains the events but leaves the starts recorded past its point, which
    /// nothing will take: the replay no longer follows the record, so they
    /// would hold the replay short of its end for good (no default
    /// priorities, `Scheduler::release_replay_priorities_if_exhausted`, and
    /// every unrecorded call a desync). `untaken_background_starts` still
    /// reports them.
    pub(super) fn has_events_left(&self) -> bool {
        !self.released()
            && (self.cursor.peek().is_some()
                || !self.pending_starts.is_empty()
                || !self.background_starts.is_empty())
    }

    /// Where the replay is: the number of the event it traces next, and the
    /// record's position, the index of the next recorded event to match
    /// (`events_popped + skipped`, the position that places the background
    /// starts) (<https://github.com/rrnewton/hermit/issues/4876>).
    pub(super) fn position_now(&self) -> ReplayPosition {
        ReplayPosition {
            event: self.traced_event_count,
            record_event: self.record_position(),
        }
    }

    /// Every recorded background start the replay did not take, in record
    /// order (<https://github.com/rrnewton/hermit/issues/4876>): those it
    /// passed (the next event came, or a fast-forward skipped them), with
    /// where it was then, and those still due or not yet due, which it never
    /// reached, those a release of the schedule left included
    /// (`has_events_left`).
    pub(super) fn untaken_background_starts(&self) -> Vec<UntakenStart> {
        let mut untaken = self.passed_starts.clone();
        untaken.extend(
            self.pending_starts
                .iter()
                .map(|&(thread, call, after)| UntakenStart {
                    thread,
                    call,
                    after,
                    passed_at: None,
                }),
        );
        untaken.extend(self.background_starts.iter().flat_map(|(after, starts)| {
            starts.iter().map(|&(thread, call)| UntakenStart {
                thread,
                call,
                after: *after,
                passed_at: None,
            })
        }));
        // Passed starts were due before those still waiting, but sort to
        // keep record order whatever the mix.
        untaken.sort_by_key(|start| start.after);
        untaken
    }

    /// How many recorded background starts the replay has not taken, due or
    /// not yet due: at the end of a run, starts it left out.
    #[cfg(test)]
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

    /// Record a fatal desync for the ERROR line and stop the replay.
    fn stop_fatal(
        &mut self,
        thread: DetTid,
        event: u64,
        expected: String,
        observed: String,
    ) -> ReplayAction {
        self.fatal_desync = Some((thread, event, expected, observed));
        ReplayAction::Stop(StopReason::FatalDesync)
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
        self.last_consumed = None;
        if self.cursor.is_empty() && self.release.is_none() && self.exhausted_from.is_none() {
            self.exhausted_from = Some(current_ix);
        }
        // The recording began its due background starts before this event. A
        // start still due was not taken: the call the recording began here is
        // not waiting to begin, so the replay has left its recording. It
        // counts as a hard desync of the start's thread, and will not be taken.
        let untaken = std::mem::take(&mut self.pending_starts);
        let passed_at = ReplayPosition {
            event: current_ix,
            record_event: self.record_position(),
        };
        for &(thread, call, after) in &untaken {
            tracing::warn!(
                "[tracereplay] the recorded background start of thread {}'s call {} before event #{} was not taken",
                thread,
                call,
                current_ix
            );
            self.desync_counts.entry(thread).or_default().hard += 1;
            self.passed_starts.push(UntakenStart {
                thread,
                call,
                after,
                passed_at: Some(passed_at),
            });
        }
        if !untaken.is_empty() && self.die_on_desync {
            let (thread, call, _) = untaken[0];
            return self.stop_fatal(
                thread,
                current_ix,
                format!("recorded start of thread {thread}'s call {call}"),
                format!("thread {thread} ran past it"),
            );
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
                if self.first_desync.is_none() {
                    self.first_desync = Some(FirstDesync {
                        event: current_ix,
                        record_event: self.record_position(),
                        observed: observed.dettid,
                        expected: expected.dettid,
                        hard: is_hard_desync(observed, &expected),
                    });
                }
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
                        return self.stop_fatal(
                            mytid,
                            current_ix,
                            format!("{expected}"),
                            format!("{observed}"),
                        );
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
                if self.retire_skipped_starts(ix as u64, current_ix) > 0 && self.die_on_desync {
                    return self.stop_fatal(
                        mytid,
                        current_ix,
                        "recorded starts skipped by the resync".to_owned(),
                        format!("{observed}"),
                    );
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
                self.last_consumed = Some(self.schedule_len - self.cursor.len() as u64);
                let _ = self.pop_next();
                self.unmatched_run = 0;
            }

            let action = self.check_context_switch(observed, !in_hard_desync_mode);
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
    /// stops making progress for good. Dropping the rest of the schedule,
    /// the background starts recorded past this point with it
    /// (`has_events_left`), ends the replay as running out of it does: the
    /// scheduler gives every
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
            // The caller moves the record position past the dropped events
            // (`retire_skipped_starts`), which places the background starts.
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

    /// `popped`: the replay popped a recorded event for `observed`, not only
    /// on an exact match but also on a soft desync (the same operation with
    /// other counts or times) and on the event a fast-forward resynchronized
    /// to (`observe_event`). Each leaves the cursor at the recording's event
    /// after one of this thread's. Only then may the reply let the thread
    /// take its next end without a stop, and only when that end is the
    /// observing thread's own next event: a context switch hands the length
    /// to another thread (or to the grant), and a hard desync, which pops
    /// nothing, leaves a head that names no end of this thread's.
    fn check_context_switch(&mut self, observed: &SchedEvent, popped: bool) -> ReplayAction {
        // We run after the pop of any matched action, so the next action in replay represents the
        // FUTURE not the PRESENT/PAST.
        //
        // A recorded background start due now is the schedule's next
        // decision, before its next event or after its last one: its thread
        // runs next only to begin its call (`take_background_start`), with no
        // slice length (`next_resume`).
        let next = self.next_slice_end();
        if let Some((next_tid, timeslice)) = next {
            let mytid = observed.dettid;
            let time = observed.end_time.expect("timestamps required for now");
            let is_prehook = matches!(observed.op, Op::Syscall(_, SyscallPhase::Prehook));
            let with_a_stop = timeslice.map(ReplayedSliceEnd::with_a_stop);

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
                    with_a_stop,
                )
            } else if popped {
                // We're still running the next event.
                ReplayAction::Continue(timeslice)
            } else {
                ReplayAction::Continue(with_a_stop)
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
                stopless: false,
                instruction_offset: None,
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
                stopless: false,
                instruction_offset: None,
                op: Op::Syscall(sys_no, SyscallPhase::Posthook),
            }
        }

        pub fn other(&mut self, end_rip: usize, tid: DetTid) -> SchedEvent {
            SchedEvent {
                count: 1,
                dettid: tid,
                start_rip: None,
                end_time: Some(self.time.as_nanos()),
                stopless: false,
                instruction_offset: None,
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
                stopless: false,
                instruction_offset: None,
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

    /// A batch of branches observed without the offset its recording carries
    /// (a replay that did not stop the thread where its recording did), or
    /// with another one, is a hard desync that names the offset, and a fatal
    /// one under `--die-on-desync`.
    #[test]
    fn a_missed_instruction_offset_is_a_hard_desync() {
        let offset = detcore_model::schedule::InstructionOffset {
            rcb_target: 120,
            instructions: 6,
        };
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.branch(123, tid(3)).with_instruction_offset(Some(offset));
        let missed = SchedEvent {
            instruction_offset: None,
            ..recorded
        };
        let moved = SchedEvent {
            instruction_offset: Some(detcore_model::schedule::InstructionOffset {
                instructions: 5,
                ..offset
            }),
            ..recorded
        };
        for observed in [&missed, &moved] {
            assert!(is_desync(observed, &recorded));
            assert!(is_hard_desync(observed, &recorded));
            assert_eq!(
                hard_desync_fields(observed, &recorded),
                vec!["instruction_offset"]
            );
            assert!(compare_desync(observed, &recorded).starts_with("OFFSET-"));
        }
        // And in the other direction: an offset the recording does not have.
        assert!(is_hard_desync(&recorded, &missed));
        assert!(!is_desync(&recorded, &recorded));

        let mut replayer = Replayer::new(vec![recorded.clone(), b.branch(1, tid(5))]);
        replayer.die_on_desync = true;
        assert_eq!(
            replayer.observe_event(&missed),
            ReplayAction::Stop(StopReason::FatalDesync)
        );
    }

    /// A recorded preemption's instruction offset comes with the timeslice
    /// the replayer derives from its batch of branches, for the thread that
    /// keeps running and for one a context switch hands the turn to, and never
    /// without that timeslice.
    #[test]
    fn a_recorded_instruction_offset_comes_with_its_timeslice() {
        let offset = detcore_model::schedule::InstructionOffset {
            rcb_target: 120,
            instructions: 6,
        };
        let mut b = builder::SchedTestBuilder::new();
        let continuing = vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)).with_instruction_offset(Some(offset)),
            b.other(SOME_RIP, tid(3)),
            b.branch(1, tid(5)),
        ];
        let mut replayer = Replayer::new(continuing.clone());
        assert_eq!(replayer.next_instruction_offset(), None);
        assert_eq!(
            replayer.observe_event(&continuing[0]),
            ReplayAction::Continue(Some(ReplayedSliceEnd {
                length: LogicalTime::from_rcbs(123),
                stopless: false,
            }))
        );
        assert_eq!(replayer.next_instruction_offset(), Some(offset));
        assert_eq!(
            replayer.next_resume(),
            Some((tid(3), Some(LogicalTime::from_rcbs(123))))
        );

        let switching = vec![
            b.other(SOME_RIP, tid(5)),
            b.branch(123, tid(3)).with_instruction_offset(Some(offset)),
            b.other(SOME_RIP, tid(3)),
            b.branch(1, tid(5)),
        ];
        let mut replayer = Replayer::new(switching.clone());
        assert_eq!(
            replayer.observe_event(&switching[0]),
            ReplayAction::ContextSwitch(
                true,
                tid(3),
                Some(ReplayedSliceEnd {
                    length: LogicalTime::from_rcbs(123),
                    stopless: false,
                })
            )
        );
        assert_eq!(replayer.next_instruction_offset(), Some(offset));

        // As a recording produces it when the thread kept running after the
        // slice end: the offset timer's batch, its `OtherInstructions`
        // event, then a syscall of the same thread at once. The batch keeps
        // its timeslice and offset (`resolve_time_to_run`), as a timer stop
        // with its RIP does, so the replay stops the thread where the
        // recording did before it runs on to the syscall.
        let continued_to_a_syscall = vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)).with_instruction_offset(Some(offset)),
            b.other(SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::write, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(continued_to_a_syscall.clone());
        assert_eq!(
            replayer.observe_event(&continued_to_a_syscall[0]),
            ReplayAction::Continue(Some(ReplayedSliceEnd {
                length: LogicalTime::from_rcbs(123),
                stopless: false,
            }))
        );
        assert_eq!(replayer.next_instruction_offset(), Some(offset));
    }
    use detcore_model::schedule::InstructionPointer;
    use pretty_assertions::assert_eq;
    use reverie::syscalls::Sysno;

    use super::*;

    const SOME_RIP: usize = 1234566;

    /// A replayed slice end at a stop, `branches` ahead.
    fn stop_after(branches: u64) -> ReplayedSliceEnd {
        ReplayedSliceEnd {
            length: LogicalTime::from_rcbs(branches),
            stopless: false,
        }
    }

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

    /// Schedule positions follow the recording: an observed event the
    /// schedule does not hold neither takes a position nor shifts the later
    /// ones, a recorded event the replay skips to resync takes no observed
    /// event, and positions go on past the schedule's end once its last event
    /// is consumed.
    #[test]
    fn stacktrace_positions_follow_the_schedule_and_continue_past_its_end() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.syscall_pre(Sysno::read, SOME_RIP, tid(3)),
            b.syscall_post(Sysno::read, SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::write, SOME_RIP, tid(3)),
            b.syscall_post(Sysno::write, SOME_RIP, tid(3)),
            b.syscall_pre(Sysno::close, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)

        let _ = replayer.observe_event(&schedule[0]);
        assert_eq!(replayer.stacktrace_position(0), Some(0));
        // A polling retry the recording did not make.
        let retry = SchedEvent {
            op: Op::Syscall(Sysno::read, SyscallPhase::Polling),
            ..schedule[0].clone()
        };
        let _ = replayer.observe_event(&retry);
        assert_eq!(replayer.stacktrace_position(1), None);
        let _ = replayer.observe_event(&schedule[1]);
        assert_eq!(replayer.stacktrace_position(2), Some(1));
        // The replay skips recorded positions 2 and 3 to resync on 4.
        let _ = replayer.observe_event(&schedule[4]);
        assert_eq!(replayer.stacktrace_position(3), Some(4));

        // Past the end, positions go on from the schedule's length.
        let after = b.syscall_post(Sysno::close, SOME_RIP, tid(3));
        let _ = replayer.observe_event(&after);
        assert_eq!(replayer.stacktrace_position(4), Some(5));
        let _ = replayer.observe_event(&b.branch(9, tid(5)));
        assert_eq!(replayer.stacktrace_position(5), Some(6));
        assert_eq!(replayer.schedule_len, 5);
    }

    /// A released schedule consumes no further position, and its events
    /// after the release take none past its end either: the positions the
    /// release dropped were never consumed.
    #[test]
    fn a_released_schedule_takes_no_further_stacktrace_position() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded = b.other(SOME_RIP, tid(3));
        let spinner = b.other(SOME_RIP + 8, tid(5));
        let mut replayer = Replayer::new([recorded.clone(), recorded.clone(), recorded]);
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
        for _ in 0..UNMATCHED_EVENTS_BEFORE_RELEASE {
            let _ = replayer.observe_event(&spinner);
        }
        assert!(replayer.released());
        let after = UNMATCHED_EVENTS_BEFORE_RELEASE;
        let _ = replayer.observe_event(&spinner);
        assert_eq!(replayer.stacktrace_position(after), None);
        let _ = replayer.observe_event(&spinner);
        assert_eq!(replayer.stacktrace_position(after + 1), None);
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
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)

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
            replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
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

    /// Every recorded background start the replay does not take is listed, in
    /// record order, with its thread, call and place in the record, and where
    /// the replay passed it: when the next event comes, when a fast-forward
    /// skips it, or never, for a start still due when the replay ends and one
    /// the replay never reached (https://github.com/rrnewton/hermit/issues/4876).
    #[test]
    fn untaken_background_starts_name_their_place_in_the_record() {
        // Ten recorded events, of which the replay runs five, with the third
        // skipped: every start lies within the record, as a valid record's
        // must (`PreemptionRecord::validate`).
        let mut b = builder::SchedTestBuilder::new();
        let ten = strip_times((1..=10).map(|n| b.branch(10 * n, tid(3))).collect());
        let start = |after, raw, call| crate::preemptions::BackgroundStart {
            after,
            thread: tid(raw),
            call,
        };
        let mut replayer = Replayer::new(ten.clone()).with_background_starts(
            &[
                start(1, 5, 1),
                start(3, 7, 1),
                start(5, 9, 1),
                start(9, 11, 1),
            ],
            true,
        );
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
        // The start after event 0 is due, and event #1 comes without it.
        replayer.observe_event(&ten[0]);
        replayer.observe_event(&ten[1]);
        // Event #2 is the record's fourth: the third is fast-forwarded past,
        // and the start before the fourth with it.
        replayer.observe_event(&ten[3]);
        // The record's fifth event makes the start after it due; the replay
        // ends there, short of the start before the record's event #9.
        replayer.observe_event(&ten[4]);
        let at = |event, record_event| {
            Some(ReplayPosition {
                event,
                record_event,
            })
        };
        let untaken = |thread, after, passed_at| UntakenStart {
            thread: tid(thread),
            call: 1,
            after,
            passed_at,
        };
        assert_eq!(
            replayer.untaken_background_starts(),
            [
                untaken(5, 1, at(1, 1)),
                untaken(7, 3, at(2, 2)),
                untaken(9, 5, None),
                untaken(11, 9, None),
            ]
        );
        assert_eq!(
            replayer.position_now(),
            ReplayPosition {
                event: 4,
                record_event: 5
            }
        );
    }

    /// A replay that releases its schedule has nothing left to replay, though
    /// background starts recorded past the release point remain queued: none
    /// is due, none names the thread to run next, and none can be taken. Each
    /// is still reported as untaken, never reached. A due start is treated
    /// alike; a release never finds one, since `observe_event` takes the due
    /// starts before it can release, so the test places one by hand.
    #[test]
    fn a_released_schedule_has_no_events_left_though_starts_remain() {
        let mut b = builder::SchedTestBuilder::new();
        let recorded: Vec<SchedEvent> = (0..4).map(|_| b.other(SOME_RIP, tid(3))).collect();
        let spinner = b.other(SOME_RIP + 8, tid(5));
        let start = |after, raw, call| crate::preemptions::BackgroundStart {
            after,
            thread: tid(raw),
            call,
        };
        let mut replayer = Replayer::new(recorded.clone())
            .with_background_starts(&[start(2, 7, 1), start(4, 9, 1)], true);
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
        assert_eq!(
            replayer.observe_event(&recorded[0]),
            ReplayAction::Continue(None)
        );
        for _ in 0..UNMATCHED_EVENTS_BEFORE_RELEASE {
            replayer.observe_event(&spinner);
        }
        assert!(replayer.released());
        assert_eq!(replayer.unconsumed_background_starts(), 2);
        assert!(!replayer.has_events_left());
        assert_eq!(replayer.pending_background_start(), None);
        assert_eq!(replayer.next_resume(), None);
        assert!(!replayer.take_background_start(tid(7), 1));

        replayer.pending_starts.push_back((tid(11), 1, 1));
        assert!(!replayer.has_events_left());
        assert_eq!(replayer.pending_background_start(), None);
        assert_eq!(replayer.next_resume(), None);
        assert!(!replayer.take_background_start(tid(11), 1));
        assert_eq!(replayer.unconsumed_background_starts(), 3);

        let never_reached = |thread, after| UntakenStart {
            thread: tid(thread),
            call: 1,
            after,
            passed_at: None,
        };
        assert_eq!(
            replayer.untaken_background_starts(),
            [
                never_reached(11, 1),
                never_reached(7, 2),
                never_reached(9, 4)
            ]
        );
        assert_eq!(
            replayer.position_now(),
            ReplayPosition {
                event: 1 + UNMATCHED_EVENTS_BEFORE_RELEASE,
                record_event: 1
            }
        );
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
        replayer.retire_skipped_starts(2, 0);
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
        // one thread with a start recorded after the fourth and one after the
        // last, and the second observed first. The replay drops the first
        // event, follows the rest, takes each start when it comes due, and
        // then is exhausted. The dropped event counts toward the position
        // (`skipped`), so the start after the last event comes due when that
        // event is consumed rather than waiting for ever.
        let mut b = builder::SchedTestBuilder::new();
        let five = strip_times(vec![
            b.branch(10, tid(3)),
            b.branch(20, tid(3)),
            b.branch(30, tid(3)),
            b.branch(40, tid(3)),
            b.branch(50, tid(3)),
        ]);
        let mut resynced = Replayer::new(five.clone())
            .with_background_starts(&[start(4, 3, 1), start(5, 3, 2)], true);
        resynced.observe_event(&five[1]);
        assert_eq!(resynced.desync_counts[&tid(3)].fast_forwards, 1);
        assert_eq!(resynced.cursor.peek(), Some(&five[2]));
        resynced.observe_event(&five[2]);
        resynced.observe_event(&five[3]);
        assert_eq!(resynced.pending_background_start(), Some((tid(3), 1)));
        assert!(resynced.take_background_start(tid(3), 1));
        resynced.observe_event(&five[4]);
        assert_eq!(resynced.pending_background_start(), Some((tid(3), 2)));
        assert_eq!(resynced.next_resume(), Some((tid(3), None)));
        assert!(resynced.has_events_left());
        assert!(resynced.take_background_start(tid(3), 2));
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
            ReplayAction::Continue(Some(stop_after(123)))
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
            ReplayAction::Continue(Some(stop_after(123)))
        );
    }

    /// A recorded end's RIP is compared exactly unless the recording took
    /// that end without a stop: a replay that ends a stopped end at another
    /// instruction is a hard desync naming `end_rip`, while at an end taken
    /// without a stop, where the recording has no RIP of its own, any RIP
    /// the replay reads (or none) is not one.
    #[test]
    fn a_recorded_end_rip_is_compared_exactly_unless_the_end_was_stopless() {
        let mut b = builder::SchedTestBuilder::new();
        let mut recorded = b.branch(100, tid(3));
        recorded.end_rip = InstructionPointer::new(0x401000);
        let mut elsewhere = recorded.clone();
        elsewhere.end_rip = InstructionPointer::new(0x402000);
        assert!(is_hard_desync(&elsewhere, &recorded));
        assert_eq!(hard_desync_fields(&elsewhere, &recorded), vec!["end_rip"]);
        assert!(!is_hard_desync(&recorded.clone(), &recorded));

        let stopless = b.branch(100, tid(3)).taken_without_stop();
        let mut stopped_there = stopless.clone();
        stopped_there.stopless = false;
        stopped_there.end_rip = InstructionPointer::new(0x402000);
        assert!(!is_hard_desync(&stopped_there, &stopless));
        assert!(hard_desync_fields(&stopped_there, &stopless).is_empty());
        assert!(!is_hard_desync(&stopless.clone(), &stopless));
    }

    /// A lone thread's slice end the recording took without a stop is
    /// replayed as a slice end even when its own check-in follows with no
    /// branch between: the unmarked shape (branches, then the thread's
    /// OtherInstructions and its syscall) reaches a check-in and gets no end
    /// at all. A schedule replay re-records below
    /// `STOPLESS_SLICE_ENDS_FORMAT`, where that following check-in would hide
    /// the end without its mark, so this replay takes it with a stop instead.
    #[test]
    fn a_stopless_end_is_a_slice_end_whatever_follows() {
        for stopless in [true, false] {
            let mut b = builder::SchedTestBuilder::new();
            let schedule = vec![
                b.other(SOME_RIP, tid(3)),
                SchedEvent {
                    stopless,
                    ..b.branch(123, tid(3))
                },
                b.other(SOME_RIP, tid(3)),
                b.syscall_pre(Sysno::write, SOME_RIP, tid(3)),
            ];
            let mut replayer = Replayer::new(schedule.clone());
            let end = stopless.then_some(ReplayedSliceEnd {
                length: LogicalTime::from_rcbs(123),
                stopless: false,
            });
            assert_eq!(
                replayer.observe_event(&schedule[0]),
                ReplayAction::Continue(end),
                "stopless: {stopless}"
            );
        }
    }

    /// A marked end that stays visible in a lowered re-record without its
    /// mark (a following Branch, another thread's event, or the record's own
    /// end) may still be taken without a stop.
    #[test]
    fn a_visible_stopless_end_may_still_be_taken_without_a_stop() {
        for following in ["branch", "other thread", "record end"] {
            let mut b = builder::SchedTestBuilder::new();
            let mut schedule = vec![
                b.other(SOME_RIP, tid(3)),
                b.branch(123, tid(3)).taken_without_stop(),
            ];
            match following {
                "branch" => schedule.push(b.branch(7, tid(3))),
                "other thread" => schedule.push(b.other(SOME_RIP, tid(5))),
                "record end" => {}
                _ => unreachable!(),
            }
            let mut replayer = Replayer::new(schedule.clone());
            assert_eq!(
                replayer.observe_event(&schedule[0]),
                ReplayAction::Continue(Some(ReplayedSliceEnd {
                    length: LogicalTime::from_rcbs(123),
                    stopless: true,
                })),
                "following: {following}"
            );
        }
    }

    /// In a run of marked ends that ends at the thread's own check-in, only
    /// the last end keeps its stop: every earlier end stays visible in a
    /// lowered re-record without its mark, because a Branch follows it
    /// (`resolve_time_to_run`). The skipped run is still stopped at that
    /// last end, at the check-in that processes it
    /// (`Detcore::catch_up_lone_slice_ends`).
    #[test]
    fn only_the_last_end_of_a_run_that_ends_at_its_own_check_in_keeps_its_stop() {
        let mut b = builder::SchedTestBuilder::new();
        let schedule = vec![
            b.other(SOME_RIP, tid(3)),
            b.branch(123, tid(3)).taken_without_stop(),
            b.branch(7, tid(3)).taken_without_stop(),
            b.other(SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(schedule.clone());
        assert_eq!(
            replayer.observe_event(&schedule[0]),
            ReplayAction::Continue(Some(ReplayedSliceEnd {
                length: LogicalTime::from_rcbs(123),
                stopless: true,
            }))
        );
        assert_eq!(
            replayer.observe_event(&schedule[1]),
            ReplayAction::Continue(Some(ReplayedSliceEnd {
                length: LogicalTime::from_rcbs(7),
                stopless: false,
            }))
        );
    }

    /// A replay that stopped at an end its recording took without a stop
    /// (its thread did not run alone there) matches it: the stop's RIP is
    /// one the recording could not have. Taking an end without a stop where
    /// the recording stopped is a desync, because the RIP it checks is gone.
    #[test]
    fn a_stop_where_the_recording_had_none_matches_but_not_the_reverse() {
        let mut b = builder::SchedTestBuilder::new();
        let stopless = b.branch(100, tid(3)).taken_without_stop();
        let stopped = SchedEvent {
            end_rip: std::num::NonZeroUsize::new(SOME_RIP),
            stopless: false,
            instruction_offset: None,
            ..stopless.clone()
        };
        assert!(events_match(&stopped, &stopless));
        assert!(!is_hard_desync(&stopped, &stopless));
        assert!(hard_desync_fields(&stopped, &stopless).is_empty());
        assert!(!events_match(&stopless, &stopped));
        assert_eq!(hard_desync_fields(&stopless, &stopped), vec!["end_rip"]);
    }

    /// Only the thread whose own next recorded event is a stopless end,
    /// right after its event matched, may take that end without a stop. A
    /// context switch hands the end's length to another thread, and an
    /// unmatched event leaves the cursor at an end this thread may not have
    /// reached the recording's way: both take the end with a stop.
    #[test]
    fn a_stopless_end_goes_only_to_its_own_thread_after_a_match() {
        let mut b = builder::SchedTestBuilder::new();
        let switch = vec![
            b.other(SOME_RIP, tid(5)),
            b.branch(100, tid(3)).taken_without_stop(),
        ];
        let mut replayer = Replayer::new(switch.clone());
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
        assert_eq!(
            replayer.observe_event(&switch[0]),
            ReplayAction::ContextSwitch(true, tid(3), Some(stop_after(100)))
        );

        let mut b = builder::SchedTestBuilder::new();
        let unmatched = vec![
            b.branch(100, tid(3)).taken_without_stop(),
            b.syscall_pre(Sysno::write, SOME_RIP, tid(3)),
        ];
        let mut replayer = Replayer::new(unmatched);
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
        let observed = b.syscall_pre(Sysno::read, SOME_RIP, tid(3));
        assert_eq!(
            replayer.observe_event(&observed),
            ReplayAction::Continue(Some(stop_after(100)))
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
            ReplayAction::Continue(Some(stop_after(123)))
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
                ReplayAction::ContextSwitch(false, tid(3), Some(stop_after(123)))
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
        replayer.die_on_desync = false; // lenient accounting (dh-mvz7y)
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
