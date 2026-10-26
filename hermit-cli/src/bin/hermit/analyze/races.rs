/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Names the two conflicting operations whose order decides the outcome.
//!
//! Analyze finds the scheduling decision (event S) after which the guest
//! meets its target, and replays the schedule with it (the target) and
//! without it (the baseline). Each replay can record an access trace
//! ([`detcore_model::access_trace`]). [`find_racing_accesses`] compares the
//! two traces from S on and names the first pair of conflicting syscalls, on
//! two threads, whose order flips, with what each returned in each order and
//! whether an edge Hermit observes orders them. It is a pure function of the
//! two traces.
//!
//! The search starts from the operation that went wrong:
//! 1. Align: `(tid, nth)` names one operation in both runs while its sysno and
//!    arguments agree; a thread's alignment ends at its first call that
//!    differs.
//! 2. Victim: an aligned call whose result, returned bytes or reached open
//!    file differs, preferring the outcome thread. A thread whose control flow
//!    later changes contributes its last such call before the change, the one
//!    that steered it; any other thread its first.
//! 3. Partner: a call of another thread that conflicts with the victim on a
//!    resource it touched (an fd slot, a cursor, a file range, a channel, a
//!    futex word, a memory range, a timer, or the victim's own thread or
//!    process) and whose order with it flips: the latest such call before the
//!    victim, or else the earliest after it.
//! 4. With no such pair: the first two timed waits (then clock reads) on two
//!    threads whose return order flips.
//!
//! Two calls are ordered by their entry and return events: a pair flips when
//! one call enters before the other enters (or returns) in one run and after
//! it in the other. A call that is absent, or never returns, sorts after
//! every event.
//!
//! Window lemma: the schedules agree before S, so a call that ended before S
//! holds the same events in both runs, and a pair whose order flips lies at
//! or after S. Every edge runs forward in the serialized order, so only edges
//! between the two accesses can order them, and happens-before is one forward
//! pass over those edges plus program order. No vector clock is kept.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use detcore_model::access_trace::Access;
use detcore_model::access_trace::AccessTrace;
use detcore_model::access_trace::ChannelId;
use detcore_model::access_trace::ChannelKind;
use detcore_model::access_trace::ChannelOp;
use detcore_model::access_trace::EdgeKind;
use detcore_model::access_trace::EventIndex;
use detcore_model::access_trace::EventRef;
use detcore_model::access_trace::FdOp;
use detcore_model::access_trace::LifetimeOp;
use detcore_model::access_trace::SignalDelivery;
use detcore_model::access_trace::SignalRecord;
use detcore_model::access_trace::SyscallRecord;
use detcore_model::access_trace::Task;
use detcore_model::access_trace::TimerId;
use detcore_model::access_trace::TimerOp;
use detcore_model::access_trace::TraceRecord;
use detcore_model::fd::FdSlot;
use detcore_model::fd::OpenFileId;
use detcore_model::futex::FutexID;
use detcore_model::futex::SharedMemoryObjectId;
use detcore_model::pid::DetTid;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

/// What the target and baseline continuations are compared over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// S: the first schedule event at which the two runs differ.
    pub decision: EventIndex,
    /// O: where the target meets its outcome; `None` means the end of the
    /// target trace.
    pub outcome: Option<OutcomeRef>,
}

/// The point at which the target meets its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutcomeRef {
    /// The event (a fatal signal, the matching output, the exit).
    pub event: EventIndex,
    /// The thread that produced it.
    pub thread: DetTid,
    /// What it was, for the report.
    pub text: String,
}

/// What the finder concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RaceStatus {
    /// Two conflicting accesses whose order flips.
    Found,
    /// No conflicting pair; two timed waits or clock reads whose return
    /// order flips.
    Timing,
    /// The traces cover the window and hold neither kind of pair.
    NoneFound,
    /// The traces cannot answer.
    Unavailable,
}

/// Which event of a syscall the order is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessPoint {
    /// Its entry (prehook event): when it used the resource.
    Entry,
    /// Its return (posthook event): when it finished waiting, or survived.
    Return,
}

/// One side of a reported pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RacingAccess {
    /// The calling thread.
    pub thread: DetTid,
    /// The thread's syscall count, which names the call in both runs.
    pub nth: u64,
    /// Which event of the call is ordered.
    pub at: AccessPoint,
    /// That event in the target; `None` for a return that never happened.
    pub event: Option<EventIndex>,
    /// That event in the baseline; `None` when the call did not run there or
    /// did not return.
    pub baseline_event: Option<EventIndex>,
    /// The call with its arguments, `lseek(6, 2048, SEEK_SET)`.
    pub call: String,
    /// What it returned in the target (`?` when it did not return).
    pub result: String,
    /// What it returned in the baseline; `None` when the baseline has no
    /// matching call.
    pub baseline_result: Option<String>,
    /// Why the baseline has no matching call when the thread ran on: its
    /// calls differ from its call with this `nth` on. `None` when the call
    /// matched, or the thread made no call with this `nth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_differs_from: Option<u64>,
    /// The thread whose kill, exit or exec ended this call's thread before
    /// the call in the target, where it does not run; it runs in the
    /// baseline only (`event` is `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_first_by: Option<DetTid>,
}

/// Kinds of resource, as the report names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Fd,
    OpenFile,
    FileContents,
    Channel,
    Futex,
    Memory,
    Timer,
    Lifetime,
}

/// The resource a pair conflicts on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceText {
    /// Its kind.
    pub kind: ResourceKind,
    /// `fd 6 of process 3`.
    pub text: String,
}

/// How the two accesses are placed in each run, by index into
/// [`RacingAccesses::accesses`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RaceOrder {
    /// Always `[0, 1]`: the accesses are listed in target order.
    pub target: [usize; 2],
    /// Their order in the baseline; an access without a baseline event sorts
    /// last.
    pub baseline: [usize; 2],
}

/// Kinds of happens-before edge: the ones Detcore records, and the data
/// edges derived here from syscall results.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
#[serde(rename_all = "snake_case")]
pub enum HbEdgeKind {
    FutexWake,
    FutexRequeue,
    Create,
    ExitJoin,
    ChildExit,
    Signal,
    /// The write that put a byte in a pipe, FIFO or stream socketpair, to
    /// the read that returned it.
    ChannelBytes,
    /// A datagram or seqpacket send, to the receive that returned it.
    Datagram,
    /// An eventfd write, to the read that consumed its count.
    Eventfd,
}

/// One happens-before edge.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub struct HbEdge {
    /// The releasing side.
    pub from: EventRef,
    /// The acquiring side.
    pub to: EventRef,
    /// What kind of synchronization.
    pub kind: HbEdgeKind,
}

/// Whether edges order the first access of a pair before the second.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HbVerdict {
    /// An edge chain leads from the first access to the second.
    pub ordered: bool,
    /// That chain, first edge first; empty when unordered.
    pub edges: Vec<HbEdge>,
}

/// The verdict in each run, each from the access that comes first there.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HappensBefore {
    /// `None` when the later access has no event in the target.
    pub target: Option<HbVerdict>,
    /// `None` when one of the accesses has no event in the baseline.
    pub baseline: Option<HbVerdict>,
}

/// What a step of an fd's history did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    /// Released the number.
    Closed,
    /// Put an open file at the number.
    Installed,
    /// The victim's thread reached another open file through the number.
    LastUse,
}

/// One earlier target call on the pair's fd slot, for the report.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HistoryEvent {
    /// What it did to the slot.
    pub kind: HistoryKind,
    /// The calling thread.
    pub thread: DetTid,
    /// Its entry event.
    pub event: EventIndex,
    /// The call and its result, `close(6) = 0`.
    pub call: String,
    /// The open file it installed, released or reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_file: Option<String>,
}

/// Another flipped pair, beyond the reported one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReorderedPair {
    /// The resource it conflicts on.
    pub resource: ResourceText,
    /// The two accesses, in target order.
    pub accesses: [RacingAccess; 2],
}

/// What [`find_racing_accesses`] reports.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RacingAccesses {
    /// What was found.
    pub status: RaceStatus,
    /// The deciding event the window starts at.
    pub decision: EventIndex,
    /// Why, in one sentence or two.
    pub reason: String,
    /// The resource of a found pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceText>,
    /// The two accesses in target order, for `found` and `timing`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accesses: Vec<RacingAccess>,
    /// Their order in each run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<RaceOrder>,
    /// Whether edges order them in each run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub happens_before: Option<HappensBefore>,
    /// Earlier target calls on the pair's fd slot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<HistoryEvent>,
    /// The target's outcome, as the caller named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<OutcomeRef>,
    /// Up to [`ALSO_REORDERED_CAP`] other flipped pairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also_reordered: Vec<ReorderedPair>,
    /// Flipped pairs beyond those.
    #[serde(default)]
    pub more_reordered: u64,
    /// A trace ends before the window does, so a later access is not seen.
    pub truncated: bool,
    /// Sentences that qualify the result (each truncation, for one).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// At most this many other flipped pairs are listed.
pub const ALSO_REORDERED_CAP: usize = 3;

/// Said whenever no edge orders a pair. Hermit sees syscalls only.
pub const INVISIBLE_SYNC_CAVEAT: &str = "Uncontended locks and atomics are not visible to Hermit.";

impl RacingAccesses {
    fn bare(
        status: RaceStatus,
        decision: EventIndex,
        reason: String,
        outcome: Option<OutcomeRef>,
    ) -> Self {
        Self {
            status,
            decision,
            reason,
            resource: None,
            accesses: Vec::new(),
            order: None,
            happens_before: None,
            history: Vec::new(),
            outcome,
            also_reordered: Vec::new(),
            more_reordered: 0,
            truncated: false,
            notes: Vec::new(),
        }
    }
}

/// Find the conflicting pair whose order flips between `target` (the run
/// that meets the outcome) and `baseline` within `window`; see the module
/// documentation. `outcome_thread` is the thread that produced the outcome,
/// whose victim is preferred; it is separate from `window.outcome` because a
/// trace can name the thread without the event.
pub fn find_racing_accesses(
    target: &AccessTrace,
    baseline: &AccessTrace,
    window: Window,
    outcome_thread: Option<DetTid>,
) -> RacingAccesses {
    let Window { decision, outcome } = window;
    let unavailable =
        |reason| RacingAccesses::bare(RaceStatus::Unavailable, decision, reason, outcome.clone());
    for (name, trace) in [("target", target), ("baseline", baseline)] {
        if trace.header.from > decision {
            return unavailable(format!(
                "the {name} trace starts at event {}, after the deciding event {decision}; \
                 record both replays with --access-trace-from at or before {decision}",
                trace.header.from
            ));
        }
    }
    for (name, trace) in [("target", target), ("baseline", baseline)] {
        let last = last_event(trace);
        if trace.end.is_none() && last < decision {
            return unavailable(format!(
                "the {name} trace ends at event {last} without an end line, before the deciding \
                 event {decision}: that replay stopped before the window; rerun the analysis so \
                 it finishes"
            ));
        }
    }
    if let Some(event) = first_prefix_difference(target, baseline, decision) {
        return unavailable(format!(
            "the two traces differ at event {event}, before the deciding event {decision}, so the \
             replays did not follow one schedule up to it; rerun the analysis so both replays \
             follow the recorded schedule"
        ));
    }

    let end = outcome.as_ref().map_or(EventIndex::MAX, |o| o.event);
    let finder = Finder::new(target, baseline, decision, end);
    let mut notes = truncation_notes(target, baseline, outcome.as_ref());
    let truncated = !notes.is_empty();
    notes.extend(finder.backward_edge_notes());

    let victims = finder.victims(outcome_thread);
    let pairs = finder.flipped_pairs(&victims.ranked);
    // A lifetime pair that involves the outcome thread (as the killer or
    // the thread it ended) comes before a timing pair; any other comes
    // after it, so how far a killed child got does not push one out.
    let (outcome_lifetime, other_lifetime): (Vec<_>, Vec<_>) = if pairs.is_empty() {
        finder.lifetime_pairs().into_iter().partition(|p| {
            outcome_thread.is_some_and(|t| t == p.kill.target.tid || t == p.call.tid)
        })
    } else {
        Default::default()
    };
    let timing = || finder.timing_pair(victims.ranked.first().copied());
    let mut result = if let Some((first, rest)) = pairs.split_first() {
        finder.found(first, rest)
    } else if let Some((first, rest)) = outcome_lifetime.split_first() {
        finder.lifetime_found(first, rest)
    } else if let Some(timing) = timing() {
        finder.timing(timing)
    } else if let Some((first, rest)) = other_lifetime.split_first() {
        finder.lifetime_found(first, rest)
    } else {
        RacingAccesses::bare(
            RaceStatus::NoneFound,
            decision,
            finder.none_found_reason(&victims),
            None,
        )
    };
    result.outcome = outcome;
    result.truncated = truncated;
    result.notes.append(&mut notes);
    result
}

/// The first event of the earliest record that differs between the traces
/// among records that end before `decision`. Such records hold the same
/// events in both runs when both followed one schedule prefix.
fn first_prefix_difference(
    target: &AccessTrace,
    baseline: &AccessTrace,
    decision: EventIndex,
) -> Option<EventIndex> {
    let from = target.header.from.max(baseline.header.from);
    let prefix = |trace: &AccessTrace| -> Vec<(EventIndex, TraceRecord)> {
        trace
            .records
            .iter()
            .filter_map(|record| {
                let (first, last) = record_span(record)?;
                (first >= from && last < decision).then(|| (first, record.clone()))
            })
            .collect()
    };
    let (t, b) = (prefix(target), prefix(baseline));
    let differs = t.iter().zip(&b).find(|(x, y)| x != y);
    match differs {
        Some(((te, _), (be, _))) => Some(*te.min(be)),
        None if t.len() != b.len() => {
            let longer = if t.len() > b.len() { &t } else { &b };
            Some(longer[t.len().min(b.len())].0)
        }
        None => None,
    }
}

/// First and last schedule event of a record; `None` for a call that never
/// returned (its last event is unbounded) and for the header and end lines.
fn record_span(record: &TraceRecord) -> Option<(EventIndex, EventIndex)> {
    match record {
        TraceRecord::Syscall(sys) => sys.post.map(|post| (sys.pre, post)),
        TraceRecord::Signal(signal) => Some((signal.event, signal.event)),
        TraceRecord::Edge(edge) => Some((edge.from.event, edge.to.event)),
        TraceRecord::WatchHit(hit) => Some((hit.after, hit.after)),
        TraceRecord::Header(_) | TraceRecord::End(_) => None,
    }
}

/// The last event a trace mentions.
fn last_event(trace: &AccessTrace) -> EventIndex {
    trace
        .records
        .iter()
        .filter_map(|r| record_span(r).map(|(first, last)| first.max(last)))
        .max()
        .unwrap_or(trace.header.from)
}

/// One sentence per trace that ends before the window does.
fn truncation_notes(
    target: &AccessTrace,
    baseline: &AccessTrace,
    outcome: Option<&OutcomeRef>,
) -> Vec<String> {
    let mut notes = Vec::new();
    let target_last = last_event(target);
    let covers_outcome = outcome.is_some_and(|o| target_last >= o.event);
    if target.end.is_none() && !covers_outcome {
        let until = outcome.map_or("the end of the run".to_string(), |o| {
            format!("the outcome at event {}", o.event)
        });
        notes.push(format!(
            "The target trace ends at event {target_last} without an end line, before {until}: \
             accesses after it are not seen."
        ));
    }
    if baseline.end.is_none() {
        notes.push(format!(
            "The baseline trace ends at event {} without an end line: a call missing from it may \
             have run later.",
            last_event(baseline)
        ));
    }
    notes
}

/// Position of an event for ordering: an absent event sorts after all.
type Key = EventIndex;

const NEVER: Key = EventIndex::MAX;

fn event_of(sys: &SyscallRecord, at: AccessPoint) -> Option<EventIndex> {
    match at {
        AccessPoint::Entry => Some(sys.pre),
        AccessPoint::Return => sys.post,
    }
}

fn key(sys: Option<&SyscallRecord>, at: AccessPoint) -> Key {
    sys.and_then(|sys| event_of(sys, at)).unwrap_or(NEVER)
}

/// A call in the target and its counterpart in the baseline.
#[derive(Clone, Copy)]
struct Op<'a> {
    target: &'a SyscallRecord,
    baseline: Option<&'a SyscallRecord>,
}

/// One side of a pair: a call and which of its events is ordered.
#[derive(Clone, Copy)]
struct Side<'a> {
    op: Op<'a>,
    at: AccessPoint,
}

impl Side<'_> {
    fn target_key(&self) -> Key {
        key(Some(self.op.target), self.at)
    }

    fn baseline_key(&self) -> Key {
        key(self.op.baseline, self.at)
    }
}

/// A flipped pair, sides in target order.
struct Pair<'a> {
    sides: [Side<'a>; 2],
    /// Which side is the victim.
    victim: usize,
    resource: ResourceText,
    /// The fd slot the pair conflicts on, for its history.
    slot: Option<FdSlot>,
}

/// Victim candidates, best first, and what the threads without one did.
struct Victims<'a> {
    ranked: Vec<Op<'a>>,
    /// Threads whose control flow changed with no result difference before.
    memory_diverged: Vec<DetTid>,
}

/// Which timed calls a timing pair was drawn from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Timed {
    Wait,
    Clock,
}

/// A timing pair and what it was drawn from: two timed calls whose
/// return order flips, around a clock-read victim when there is one.
struct TimingPair<'a> {
    sides: [Side<'a>; 2],
    kinds: Timed,
    clock: Option<Op<'a>>,
}

/// A kill, exit or exec (in both runs) and a call of the thread it ended
/// that runs in the baseline only.
#[derive(Clone, Copy)]
struct LifetimePair<'a> {
    kill: Op<'a>,
    call: &'a SyscallRecord,
    task: Task,
}

/// One trace, indexed.
struct Run<'a> {
    syscalls: Vec<&'a SyscallRecord>,
    signals: Vec<&'a SignalRecord>,
    by_op: BTreeMap<(DetTid, u64), &'a SyscallRecord>,
    /// Recorded and derived edges, sorted by `from` event.
    edges: Vec<HbEdge>,
    /// Channels whose data gives no edge, from an event on.
    untracked: BTreeMap<ChannelKey, Untracked>,
    /// Recorded edges that do not run forward; they are ignored.
    backward: Vec<HbEdge>,
    /// Channels that may hold data queued before the trace starts.
    predate: BTreeSet<ChannelId>,
}

impl<'a> Run<'a> {
    fn new(trace: &'a AccessTrace) -> Self {
        let syscalls: Vec<&SyscallRecord> = trace
            .records
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Syscall(sys) => Some(sys),
                _ => None,
            })
            .collect();
        let by_op = syscalls.iter().map(|s| ((s.tid, s.nth), *s)).collect();
        let signals = trace
            .records
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Signal(signal) => Some(signal),
                _ => None,
            })
            .collect();
        let (backward, mut edges): (Vec<HbEdge>, Vec<HbEdge>) = trace
            .records
            .iter()
            .filter_map(|r| match r {
                TraceRecord::Edge(e) => Some(HbEdge {
                    from: e.from,
                    to: e.to,
                    kind: recorded_kind(e.kind),
                }),
                _ => None,
            })
            .partition(|e| e.to.event <= e.from.event);
        let data = data_edges(&syscalls);
        edges.extend(data.edges);
        edges.sort_by_key(|e| (e.from.event, e.to.event, e.from.tid, e.to.tid, e.kind));
        Self {
            syscalls,
            signals,
            by_op,
            edges,
            untracked: data.untracked,
            backward,
            predate: data.predate,
        }
    }

    /// The call of `thread` with the highest count.
    fn last_call(&self, thread: DetTid) -> Option<&'a SyscallRecord> {
        self.by_op
            .range((thread, 0)..=(thread, u64::MAX))
            .next_back()
            .map(|(_, sys)| *sys)
    }

    /// Whether the trace shows signal `signo` ending a thread of `task` by its
    /// default action at or after event `from`.
    fn fatal_signal_after(&self, task: Task, signo: i32, from: EventIndex) -> bool {
        self.signals.iter().any(|signal| {
            signal.signo == signo
                && signal.event >= from
                && signal.delivery == SignalDelivery::Fatal
                && match task {
                    Task::Thread(tid) => signal.tid == tid,
                    Task::Process(pid) => self
                        .syscalls
                        .iter()
                        .any(|sys| sys.tid == signal.tid && sys.pid == pid),
                    Task::ProcessGroup(_) => false,
                }
        })
    }

    /// One note per channel that a call between `from` and `to` wrote or
    /// read and whose data gives no edge, or may predate the trace.
    fn channel_notes_between(&self, from: EventIndex, to: EventIndex) -> BTreeSet<String> {
        let mut notes = BTreeSet::new();
        for sys in &self.syscalls {
            let at = sys.post.unwrap_or(sys.pre);
            if at < from || sys.pre > to {
                continue;
            }
            for access in &sys.accesses {
                let Access::Channel {
                    open_file,
                    id,
                    op: ChannelOp::Read | ChannelOp::Write,
                    ..
                } = access
                else {
                    continue;
                };
                let Some(untracked) = self.untracked.get(&channel_key(*open_file, *id)) else {
                    if id.is_some_and(|id| self.predate.contains(&id)) {
                        notes.insert(format!(
                            "Data through {} may include bytes queued before the trace starts, \
                             which the trace does not show being created: a read of them is \
                             credited to a later write.",
                            resource_text(access).text
                        ));
                    }
                    continue;
                };
                if untracked.since <= at {
                    let since = match untracked.since {
                        0 => String::new(),
                        e => format!(" from event {e} on"),
                    };
                    notes.insert(format!(
                        "Data through {} gives no edge{since}: {}.",
                        resource_text(access).text,
                        untracked.why
                    ));
                }
            }
        }
        notes
    }
}

fn recorded_kind(kind: EdgeKind) -> HbEdgeKind {
    match kind {
        EdgeKind::FutexWake => HbEdgeKind::FutexWake,
        EdgeKind::FutexRequeue => HbEdgeKind::FutexRequeue,
        EdgeKind::Create => HbEdgeKind::Create,
        EdgeKind::ExitJoin => HbEdgeKind::ExitJoin,
        EdgeKind::ChildExit => HbEdgeKind::ChildExit,
        EdgeKind::Signal => HbEdgeKind::Signal,
    }
}

/// A channel for data edges: the identity both ends share, or a lone end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ChannelKey {
    Shared(ChannelId),
    End(OpenFileId),
}

fn channel_key(open_file: OpenFileId, id: Option<ChannelId>) -> ChannelKey {
    id.map_or(ChannelKey::End(open_file), ChannelKey::Shared)
}

/// Why a channel's data gives no edge, and from which event.
#[derive(Debug, Clone, Copy)]
struct Untracked {
    since: EventIndex,
    why: &'static str,
}

const NO_PEER: &str = "Hermit does not record the end it is connected to";
const NO_SOCKET_TYPE: &str = "Hermit does not record its socket type";
const UNSEEN_DATA: &str = "a read returned data the trace does not show being written";

/// What [`data_edges`] derives from one trace.
struct DataEdges {
    edges: Vec<HbEdge>,
    untracked: BTreeMap<ChannelKey, Untracked>,
    /// Channels with data steps that the trace does not show being created
    /// (a pipe or socketpair opened before it, any FIFO), which may hold data
    /// queued before the trace starts.
    predate: BTreeSet<ChannelId>,
}

/// Data edges (design section 7), derived from syscall results: the write
/// that put a byte (a message, an eventfd count) in a channel, to the read
/// that returned it. For an eventfd, every write since the previous read
/// reaches a read (in semaphore mode a read consumes one of them, which the
/// trace cannot tell).
fn data_edges(syscalls: &[&SyscallRecord]) -> DataEdges {
    let (steps, mut untracked) = channel_steps(syscalls);
    let edges = replay_channels(&steps, &mut untracked);
    let opened: BTreeSet<OpenFileId> = syscalls
        .iter()
        .flat_map(|sys| &sys.accesses)
        .filter_map(|access| match access {
            Access::Fd {
                open_file: Some(open_file),
                op: FdOp::Open { .. },
                ..
            } => Some(*open_file),
            _ => None,
        })
        .collect();
    let predate = steps
        .iter()
        .map(|step| step.id)
        .filter(|id| !matches!(id, ChannelId::Pair(first) if opened.contains(first)))
        .collect();
    DataEdges {
        edges,
        untracked,
        predate,
    }
}

/// One write or read that moved data through a channel.
struct Step<'a> {
    /// When the data moved: a write's entry, a read's return.
    at: EventIndex,
    sys: &'a SyscallRecord,
    open_file: OpenFileId,
    id: ChannelId,
    kind: ChannelKind,
    write: bool,
    count: u64,
}

/// The writes and reads that moved data, in the order the data moved, and
/// the ends no step can follow: an end without an identity both ends share,
/// and a socket of unrecorded type.
fn channel_steps<'a>(
    syscalls: &[&'a SyscallRecord],
) -> (Vec<Step<'a>>, BTreeMap<ChannelKey, Untracked>) {
    let mut steps = Vec::new();
    let mut untracked = BTreeMap::new();
    for sys in syscalls {
        let Some(count) = sys.result.filter(|r| *r > 0).map(|r| r as u64) else {
            continue;
        };
        for access in &sys.accesses {
            let Access::Channel {
                open_file,
                id,
                channel,
                op,
            } = access
            else {
                continue;
            };
            let (write, at) = match op {
                ChannelOp::Write => (true, sys.pre),
                ChannelOp::Read => match sys.post {
                    Some(post) => (false, post),
                    None => continue,
                },
                ChannelOp::Shutdown | ChannelOp::Close => continue,
            };
            let why = match (id, channel) {
                (None, _) => NO_PEER,
                (Some(_), ChannelKind::Socket) => NO_SOCKET_TYPE,
                (Some(id), kind) => {
                    steps.push(Step {
                        at,
                        sys,
                        open_file: *open_file,
                        id: *id,
                        kind: *kind,
                        write,
                        count,
                    });
                    continue;
                }
            };
            untracked
                .entry(channel_key(*open_file, *id))
                .or_insert(Untracked { since: 0, why });
        }
    }
    steps.sort_by_key(|s| s.at);
    (steps, untracked)
}

/// A byte stream, message queue or counter one channel carries in one
/// direction: the channel, plus the writing end for a socketpair, whose two
/// ends both write.
type StreamKey = (ChannelId, Option<OpenFileId>);

/// Data written to one stream and read from it so far, and the writes whose
/// bytes (messages, counts) are not all read yet, by the offset they end at.
#[derive(Default)]
struct Stream {
    written: u64,
    read: u64,
    pending: VecDeque<(u64, EventRef)>,
    /// The last write, which an eventfd in semaphore mode can feed several
    /// reads from.
    last_write: Option<EventRef>,
}

/// Replays the steps per stream and gives each read an edge from every write
/// whose data it returned; a write's bytes are taken as contiguous. A read
/// that returns more than the trace shows being written (data from before the
/// trace) makes the channel's offsets unknown, and its edges stop there.
/// Data queued before the trace starts that never makes a read exceed the
/// trace's writes goes undetected: the reads are credited to later writes,
/// which can give a false "ordered" or miss an edge. Such a channel is one the
/// trace does not show being created, and gets a note.
fn replay_channels(
    steps: &[Step<'_>],
    untracked: &mut BTreeMap<ChannelKey, Untracked>,
) -> Vec<HbEdge> {
    let mut streams: BTreeMap<StreamKey, Stream> = BTreeMap::new();
    let mut edges = Vec::new();
    for step in steps {
        let key = ChannelKey::Shared(step.id);
        if untracked.contains_key(&key) {
            continue;
        }
        let two_way = matches!(
            step.kind,
            ChannelKind::StreamSocket | ChannelKind::DatagramSocket | ChannelKind::SeqpacketSocket
        );
        let edge_kind = match step.kind {
            ChannelKind::DatagramSocket | ChannelKind::SeqpacketSocket => HbEdgeKind::Datagram,
            ChannelKind::Eventfd => HbEdgeKind::Eventfd,
            _ => HbEdgeKind::ChannelBytes,
        };
        // How far a step moves the stream: bytes, or one message or count.
        let size = match edge_kind {
            HbEdgeKind::ChannelBytes => step.count,
            _ => 1,
        };
        let here = EventRef {
            tid: step.sys.tid,
            event: step.at,
        };
        if step.write {
            let stream = streams
                .entry((step.id, two_way.then_some(step.open_file)))
                .or_default();
            stream.written = stream.written.saturating_add(size);
            stream.pending.push_back((stream.written, here));
            stream.last_write = Some(here);
            continue;
        }
        // A read on one end of a socketpair takes what the other end wrote.
        let stream = streams
            .range_mut((step.id, None)..)
            .take_while(|((id, _), _)| *id == step.id)
            .find(|((_, writer), _)| !two_way || *writer != Some(step.open_file))
            .map(|(_, stream)| stream);
        // An eventfd read takes every count written since the last read. In
        // semaphore mode one write's count feeds several reads, so a read
        // with nothing new is reached from the last write.
        if edge_kind == HbEdgeKind::Eventfd
            && let Some(from) = stream
                .as_ref()
                .filter(|s| s.pending.is_empty())
                .and_then(|s| s.last_write)
        {
            edges.push(HbEdge {
                from,
                to: here,
                kind: edge_kind,
            });
            continue;
        }
        let upto = |s: &Stream| match edge_kind {
            HbEdgeKind::Eventfd => s.written,
            _ => s.read.saturating_add(size),
        };
        let Some(stream) = stream.filter(|s| s.read < upto(s) && upto(s) <= s.written) else {
            untracked.insert(
                key,
                Untracked {
                    since: step.at,
                    why: UNSEEN_DATA,
                },
            );
            continue;
        };
        let upto = upto(stream);
        while let Some(&(end, from)) = stream.pending.front() {
            edges.push(HbEdge {
                from,
                to: here,
                kind: edge_kind,
            });
            if end > upto {
                break;
            }
            stream.pending.pop_front();
            if end == upto {
                break;
            }
        }
        stream.read = upto;
    }
    edges
}

/// Whether an edge chain leads from `from` to `to`, using edges between them
/// only (the window lemma) and program order.
fn happens_before(edges: &[HbEdge], from: EventRef, to: EventRef) -> HbVerdict {
    let unordered = HbVerdict {
        ordered: false,
        edges: Vec::new(),
    };
    if to.event < from.event {
        return unordered;
    }
    // The earliest event at which each thread is reached, and the edge that
    // reached it. Edges are sorted by `from` event and every edge runs
    // forward, so a thread's entry is final before any edge leaves it.
    let mut reached: BTreeMap<DetTid, (EventIndex, Option<usize>)> = BTreeMap::new();
    reached.insert(from.tid, (from.event, None));
    let start = edges.partition_point(|e| e.from.event < from.event);
    for (index, edge) in edges.iter().enumerate().skip(start) {
        if edge.from.event > to.event {
            break;
        }
        if edge.to.event > to.event {
            continue;
        }
        let Some(&(at, _)) = reached.get(&edge.from.tid) else {
            continue;
        };
        if at > edge.from.event {
            continue;
        }
        let better = reached
            .get(&edge.to.tid)
            .is_none_or(|&(t, _)| edge.to.event < t);
        if better {
            reached.insert(edge.to.tid, (edge.to.event, Some(index)));
        }
    }
    match reached.get(&to.tid) {
        Some(&(at, mut via)) if at <= to.event => {
            let mut chain = Vec::new();
            while let Some(index) = via {
                let edge = edges[index];
                chain.push(edge);
                via = reached[&edge.from.tid].1;
            }
            chain.reverse();
            HbVerdict {
                ordered: true,
                edges: chain,
            }
        }
        _ => unordered,
    }
}

/// The two traces, aligned, and the window.
struct Finder<'a> {
    target: Run<'a>,
    baseline: Run<'a>,
    /// Per thread, the first `nth` whose sysno or arguments differ.
    split: BTreeMap<DetTid, u64>,
    decision: EventIndex,
    end: EventIndex,
}

impl<'a> Finder<'a> {
    fn new(
        target: &'a AccessTrace,
        baseline: &'a AccessTrace,
        decision: EventIndex,
        end: EventIndex,
    ) -> Self {
        let target = Run::new(target);
        let baseline = Run::new(baseline);
        // A call that entered before the decision did so in both runs alike
        // (the window lemma); where its arguments still differ they point
        // outside the guest's control (the container's first execve gets
        // Hermit's own launcher pointers), so they split nothing.
        let mut split = BTreeMap::new();
        for (&(tid, nth), t) in target.by_op.iter().filter(|(_, t)| t.pre >= decision) {
            let differs = |b: &&SyscallRecord| b.sysno != t.sysno || b.args != t.args;
            if !split.contains_key(&tid) && baseline.by_op.get(&(tid, nth)).is_some_and(differs) {
                split.insert(tid, nth);
            }
        }
        Self {
            target,
            baseline,
            split,
            decision,
            end,
        }
    }

    fn backward_edge_notes(&self) -> Vec<String> {
        [("target", &self.target), ("baseline", &self.baseline)]
            .into_iter()
            .filter_map(|(name, run)| {
                let first = run.backward.first()?;
                Some(format!(
                    "The {name} trace has {} edge(s) that do not run forward (the first from \
                     event {} to event {}); they are ignored.",
                    run.backward.len(),
                    first.from.event,
                    first.to.event
                ))
            })
            .collect()
    }

    fn access_of(&self, side: Side<'_>) -> RacingAccess {
        let Side { op, at } = side;
        let sys = op.target;
        RacingAccess {
            thread: sys.tid,
            nth: sys.nth,
            at,
            event: event_of(sys, at),
            baseline_event: op.baseline.and_then(|b| event_of(b, at)),
            call: call_text(sys),
            result: result_text(sys),
            baseline_result: op.baseline.map(result_text),
            baseline_differs_from: self
                .split
                .get(&sys.tid)
                .copied()
                .filter(|&nth| op.baseline.is_none() && nth <= sys.nth),
            ended_first_by: None,
        }
    }

    /// The call's counterpart in the baseline, while the thread's control
    /// flow agrees.
    fn counterpart(&self, sys: &SyscallRecord) -> Option<&'a SyscallRecord> {
        if self.split.get(&sys.tid).is_some_and(|&n| sys.nth >= n) {
            return None;
        }
        self.baseline
            .by_op
            .get(&(sys.tid, sys.nth))
            .copied()
            .filter(|b| b.sysno == sys.sysno && (sys.pre < self.decision || b.args == sys.args))
    }

    /// Target calls that can take part in a flip: they end at or after the
    /// decision and start no later than the outcome.
    fn in_window(&self) -> impl Iterator<Item = Op<'a>> + '_ {
        self.target
            .syscalls
            .iter()
            .filter(|s| s.post.unwrap_or(NEVER) >= self.decision && s.pre <= self.end)
            .map(|&target| Op {
                target,
                baseline: self.counterpart(target),
            })
    }

    fn victims(&self, outcome_thread: Option<DetTid>) -> Victims<'a> {
        let mut per_thread: BTreeMap<DetTid, Vec<Op<'a>>> = BTreeMap::new();
        for op in self.in_window() {
            if op.baseline.is_some_and(|b| differs(op.target, b)) {
                per_thread.entry(op.target.tid).or_default().push(op);
            }
        }
        let control_flow_changed = |tid: DetTid| {
            self.split.get(&tid).is_some_and(|&nth| {
                self.target
                    .by_op
                    .get(&(tid, nth))
                    .is_some_and(|s| s.pre <= self.end)
            })
        };
        let memory_diverged = self
            .split
            .keys()
            .filter(|&&tid| control_flow_changed(tid) && !per_thread.contains_key(&tid))
            .copied()
            .collect();
        let mut chosen = Vec::new();
        let mut rest = Vec::new();
        for (tid, mut diffs) in per_thread {
            diffs.sort_by_key(|op| op.target.nth);
            let pick = if control_flow_changed(tid) {
                diffs.len() - 1
            } else {
                0
            };
            chosen.push(diffs.remove(pick));
            rest.extend(diffs);
        }
        chosen.sort_by_key(|op| (Some(op.target.tid) != outcome_thread, op.target.pre));
        rest.sort_by_key(|op| op.target.pre);
        chosen.extend(rest);
        Victims {
            ranked: chosen,
            memory_diverged,
        }
    }

    /// Every flipped pair, best first and without repeats: each victim's
    /// partners, the one that released what the victim used first
    /// ([`releases_what_the_victim_used`]), then the latest one before it,
    /// then the earliest one after it.
    fn flipped_pairs(&self, victims: &[Op<'a>]) -> Vec<Pair<'a>> {
        let window: Vec<Op<'a>> = self.in_window().collect();
        let mut seen = BTreeSet::new();
        let mut pairs = Vec::new();
        for victim in victims {
            let mut ranked = Vec::new();
            for partner in &window {
                if partner.target.tid == victim.target.tid {
                    continue;
                }
                let Some(pair) = flip(*victim, *partner) else {
                    continue;
                };
                let names = [victim, partner].map(|op| (op.target.tid, op.target.nth));
                if !seen.insert((names[0].min(names[1]), names[0].max(names[1]))) {
                    continue;
                }
                let released = victim
                    .baseline
                    .is_some_and(|used| releases_what_the_victim_used(used, partner.target));
                let pre = partner.target.pre;
                let place = if pre < victim.target.pre {
                    (false, std::cmp::Reverse(pre), 0)
                } else {
                    (true, std::cmp::Reverse(0), pre)
                };
                ranked.push(((!released, place), pair));
            }
            ranked.sort_by_key(|(rank, _)| *rank);
            pairs.extend(ranked.into_iter().map(|(_, pair)| pair));
        }
        pairs
    }

    fn found(&self, pair: &Pair<'a>, rest: &[Pair<'a>]) -> RacingAccesses {
        let victim = pair.sides[pair.victim].op;
        let partner = pair.sides[1 - pair.victim].op;
        let when = if pair.victim == 1 { "before" } else { "after" };
        let reason = format!(
            "{}; {} on {} runs {when} it in the target and the other way round in the baseline",
            describe(victim.target, victim.baseline),
            who(partner.target),
            pair.resource.text
        );
        let also: Vec<ReorderedPair> = rest
            .iter()
            .take(ALSO_REORDERED_CAP)
            .map(|p| ReorderedPair {
                resource: p.resource.clone(),
                accesses: p.sides.map(|side| self.access_of(side)),
            })
            .collect();
        let stretch = matches!(
            pair.resource.kind,
            ResourceKind::Futex | ResourceKind::Channel | ResourceKind::Lifetime
        );
        let mut result = self.pair_result(RaceStatus::Found, reason, pair.sides, stretch);
        result.resource = Some(pair.resource.clone());
        result.history = pair
            .slot
            .map(|slot| self.fd_history(slot, pair))
            .unwrap_or_default();
        result.more_reordered = (rest.len() - also.len()) as u64;
        result.also_reordered = also;
        result
    }

    /// The fields every reported pair carries.
    fn pair_result(
        &self,
        status: RaceStatus,
        reason: String,
        sides: [Side<'a>; 2],
        stretch: bool,
    ) -> RacingAccesses {
        let [first, second] = sides;
        let swapped = second.baseline_key() < first.baseline_key();
        let in_baseline = if swapped { [second, first] } else { sides };
        let mut result = RacingAccesses::bare(status, self.decision, reason, None);
        result.accesses = sides.map(|side| self.access_of(side)).to_vec();
        result.order = Some(RaceOrder {
            target: [0, 1],
            baseline: if swapped { [1, 0] } else { [0, 1] },
        });
        let ends = [
            hb_ends(sides.map(|s| Some((s.op.target, s.at))), stretch),
            hb_ends(
                in_baseline.map(|s| s.op.baseline.map(|b| (b, s.at))),
                stretch,
            ),
        ];
        let [target, baseline] =
            [(&self.target, ends[0]), (&self.baseline, ends[1])].map(|(run, ends)| {
                ends.map(|(from, to)| {
                    (
                        happens_before(&run.edges, from, to),
                        run.channel_notes_between(from.event, to.event),
                    )
                })
            });
        let mut notes = BTreeSet::new();
        for (_, untracked) in target.iter().chain(&baseline) {
            notes.extend(untracked.iter().cloned());
        }
        result.notes = notes.into_iter().collect();
        result.happens_before = Some(HappensBefore {
            target: target.map(|(verdict, _)| verdict),
            baseline: baseline.map(|(verdict, _)| verdict),
        });
        result
    }

    /// The target's earlier calls on the pair's slot: its last close, the
    /// call that installed what it holds, and the victim thread's last use
    /// through it of another open file.
    fn fd_history(&self, slot: FdSlot, pair: &Pair<'a>) -> Vec<HistoryEvent> {
        let before = pair.sides[0].op.target.pre;
        let reached = |sys: &SyscallRecord| {
            sys.accesses.iter().find_map(|a| match a {
                Access::Fd {
                    slot: s,
                    open_file,
                    op: FdOp::Use,
                } if *s == slot => Some(*open_file),
                _ => None,
            })
        };
        let users: Vec<(DetTid, OpenFileId)> = pair
            .sides
            .iter()
            .filter_map(|s| Some((s.op.target.tid, reached(s.op.target)??)))
            .collect();
        let mut closed = None;
        let mut installed = None;
        let mut last_use = None;
        for sys in self.target.syscalls.iter().rev().filter(|s| s.pre < before) {
            // A failed open or dup installed nothing. close(2) clears the
            // table entry (file_close_fd) before anything else can fail
            // (filp_flush), so only a close that failed with EBADF released
            // nothing.
            let installed_ok = sys.result.is_some_and(|r| r >= 0);
            let released = sys.result.is_some_and(|r| r != -i64::from(libc::EBADF));
            for access in &sys.accesses {
                let Access::Fd {
                    slot: s,
                    open_file,
                    op,
                } = access
                else {
                    continue;
                };
                if *s != slot {
                    continue;
                }
                let kind = match op {
                    FdOp::Close if released && closed.is_none() => HistoryKind::Closed,
                    FdOp::Open { .. } | FdOp::Dup { .. } if installed_ok && installed.is_none() => {
                        HistoryKind::Installed
                    }
                    FdOp::Use
                        if last_use.is_none()
                            && users.iter().any(|&(tid, now)| {
                                tid == sys.tid && open_file.is_some_and(|f| f != now)
                            }) =>
                    {
                        HistoryKind::LastUse
                    }
                    _ => continue,
                };
                let path = match op {
                    FdOp::Open { path: Some(path) } => format!(" ({path})"),
                    _ => String::new(),
                };
                let event = HistoryEvent {
                    kind,
                    thread: sys.tid,
                    event: sys.pre,
                    call: format!("{} = {}", call_text(sys), result_text(sys)),
                    open_file: open_file.map(|f| format!("{f}{path}")),
                };
                match kind {
                    HistoryKind::Closed => closed = Some(event),
                    HistoryKind::Installed => installed = Some(event),
                    HistoryKind::LastUse => last_use = Some(event),
                }
            }
        }
        let mut history: Vec<HistoryEvent> = [closed, installed, last_use]
            .into_iter()
            .flatten()
            .collect();
        history.sort_by_key(|h| h.event);
        history
    }

    /// The first two timed waits on two threads whose return order flips,
    /// or else the first two timed waits or clock reads.
    /// The timed calls (waits, and clock reads too unless `waits_only`) on
    /// any thread that return inside the window, by target return.
    fn timed_returns(&self, waits_only: bool) -> Vec<Side<'a>> {
        let mut timed: Vec<Side<'a>> = self
            .in_window()
            .filter(|op| op.baseline.is_some())
            .filter(|op| {
                op.target
                    .post
                    .is_some_and(|p| p >= self.decision && p <= self.end)
            })
            .filter(|op| timed_kind(op.target).is_some_and(|k| k == Timed::Wait || !waits_only))
            .map(|op| Side {
                op,
                at: AccessPoint::Return,
            })
            .collect();
        timed.sort_by_key(|s| s.target_key());
        timed
    }

    /// The timing pair: with a clock-read `victim` (it returned a different
    /// time), the other thread's timed call whose return order with it flips
    /// (the latest before it, else the earliest after); otherwise, or when
    /// none flips, the first two timed waits on two threads whose return
    /// order flips, then the first two timed waits or clock reads.
    fn timing_pair(&self, victim: Option<Op<'a>>) -> Option<TimingPair<'a>> {
        let clock = victim.filter(|v| timed_kind(v.target) == Some(Timed::Clock));
        let around = clock.and_then(|victim| {
            let v = Side {
                op: victim,
                at: AccessPoint::Return,
            };
            let flips = |s: &&Side<'a>| {
                s.op.target.tid != victim.target.tid
                    && (s.target_key() < v.target_key()) != (s.baseline_key() < v.baseline_key())
            };
            let timed = self.timed_returns(false);
            let partner = timed
                .iter()
                .filter(|s| s.target_key() < v.target_key())
                .rfind(flips)
                .or_else(|| {
                    timed
                        .iter()
                        .filter(|s| s.target_key() > v.target_key())
                        .find(flips)
                })?;
            let sides = if partner.target_key() < v.target_key() {
                [*partner, v]
            } else {
                [v, *partner]
            };
            Some((sides, Timed::Clock))
        });
        let (sides, kinds) = around.or_else(|| {
            [Timed::Wait, Timed::Clock].into_iter().find_map(|widest| {
                first_inversion(&self.timed_returns(widest == Timed::Wait))
                    .map(|pair| (pair, widest))
            })
        })?;
        Some(TimingPair {
            sides,
            kinds,
            clock,
        })
    }

    fn timing(&self, timing: TimingPair<'a>) -> RacingAccesses {
        let TimingPair {
            sides,
            kinds,
            clock,
        } = timing;
        let what = match kinds {
            Timed::Wait => "two timed waits",
            Timed::Clock => "two timed waits or clock reads",
        };
        let victim_side = clock.filter(|victim| {
            sides
                .iter()
                .any(|side| std::ptr::eq(side.op.target, victim.target))
        });
        let first = match victim_side {
            Some(victim) => format!(
                "{} returns a different time at event {} in the two runs",
                who(victim.target),
                victim.target.post.unwrap_or(victim.target.pre)
            ),
            None => format!(
                "no conflicting kernel-visible access changes order after the deciding event {}",
                self.decision
            ),
        };
        let reason = format!(
            "{first}; the order that flips is {what}: {} and {}",
            who(sides[0].op.target),
            who(sides[1].op.target),
        );
        self.pair_result(RaceStatus::Timing, reason, sides, false)
    }

    /// Pairs of a kill, exit or exec in the target and a call its target
    /// thread makes only in the baseline: in the target the call's thread
    /// ended first; in the baseline the call entered before it. Each thread V
    /// of the ended task, other than the killer, whose last target call
    /// entered before the kill and whose baseline has its next call, entered
    /// in the window before the kill's baseline entry.
    fn lifetime_pairs(&self) -> Vec<LifetimePair<'a>> {
        let mut pairs = Vec::new();
        for kill in self.in_window() {
            let Some(kill_baseline) = kill.baseline else {
                continue;
            };
            for task in self.ended_tasks(kill.target) {
                for thread in self.threads_of(task) {
                    if thread == kill.target.tid {
                        continue;
                    }
                    let Some(last) = self.target.last_call(thread) else {
                        continue;
                    };
                    // `last.nth + 1` is V's next call only while V's calls
                    // match through `last`.
                    if self.split.get(&thread).is_some_and(|&nth| nth <= last.nth) {
                        continue;
                    }
                    let next = self.baseline.by_op.get(&(thread, last.nth + 1)).copied();
                    let Some(call) = next.filter(|call| {
                        last.pre < kill.target.pre
                            && call.pre >= self.decision
                            && call.pre < kill_baseline.pre
                    }) else {
                        continue;
                    };
                    pairs.push(LifetimePair { kill, call, task });
                }
            }
        }
        pairs.sort_by_key(|p| (p.call.pre, p.kill.target.pre));
        pairs
    }

    /// The tasks `kill` ends: an exit or exec, and a signal that kills (a
    /// SIGKILL, or one the target trace shows ending a thread of the task by
    /// its default action).
    fn ended_tasks(&self, kill: &SyscallRecord) -> Vec<Task> {
        if kill.result.is_some_and(|r| r < 0) {
            // A failed kill, exit or exec ended nothing.
            return Vec::new();
        }
        kill.accesses
            .iter()
            .filter_map(|access| match access {
                Access::Lifetime { target, op } => Some((*target, *op)),
                _ => None,
            })
            .filter(|(target, op)| match op {
                LifetimeOp::Exit | LifetimeOp::Exec => true,
                LifetimeOp::Signal { signo } => {
                    *signo == libc::SIGKILL
                        || self.target.fatal_signal_after(*target, *signo, kill.pre)
                }
                LifetimeOp::Create | LifetimeOp::Wait => false,
            })
            .map(|(target, _)| target)
            .collect()
    }

    /// The threads of `task` either trace shows. A process group is matched
    /// through the member processes the trace lists beside it.
    fn threads_of(&self, task: Task) -> BTreeSet<DetTid> {
        let all = self.target.syscalls.iter().chain(&self.baseline.syscalls);
        match task {
            Task::Thread(tid) => BTreeSet::from([tid]),
            Task::Process(pid) => all
                .filter(|sys| sys.pid == pid)
                .map(|sys| sys.tid)
                .collect(),
            Task::ProcessGroup(_) => BTreeSet::new(),
        }
    }

    fn lifetime_found(&self, pair: &LifetimePair<'a>, rest: &[LifetimePair<'a>]) -> RacingAccesses {
        let LifetimePair { kill, call, task } = *pair;
        let reason = format!(
            "{} ends thread {} before its {} in the target; in the baseline that call enters first \
             (event {})",
            who(kill.target),
            call.tid,
            call_text(call),
            call.pre
        );
        let mut result = RacingAccesses::bare(RaceStatus::Found, self.decision, reason, None);
        result.resource = Some(task_text(task));
        result.accesses = self.lifetime_accesses(pair).to_vec();
        result.order = Some(RaceOrder {
            target: [0, 1],
            baseline: [1, 0],
        });
        let baseline = hb_ends(
            [
                Some((call, AccessPoint::Entry)),
                kill.baseline.map(|b| (b, AccessPoint::Entry)),
            ],
            false,
        )
        .map(|(from, to)| {
            result
                .notes
                .extend(self.baseline.channel_notes_between(from.event, to.event));
            happens_before(&self.baseline.edges, from, to)
        });
        result.happens_before = Some(HappensBefore {
            target: None,
            baseline,
        });
        let also: Vec<ReorderedPair> = rest
            .iter()
            .take(ALSO_REORDERED_CAP)
            .map(|p| ReorderedPair {
                resource: task_text(p.task),
                accesses: self.lifetime_accesses(p),
            })
            .collect();
        result.more_reordered = (rest.len() - also.len()) as u64;
        result.also_reordered = also;
        result
    }

    /// The kill, as an access in both runs, and the call that runs in the
    /// baseline only.
    fn lifetime_accesses(&self, pair: &LifetimePair<'a>) -> [RacingAccess; 2] {
        let LifetimePair { kill, call, .. } = *pair;
        let ended = RacingAccess {
            thread: call.tid,
            nth: call.nth,
            at: AccessPoint::Entry,
            event: None,
            baseline_event: Some(call.pre),
            call: call_text(call),
            result: "does not run".to_string(),
            baseline_result: Some(result_text(call)),
            baseline_differs_from: None,
            ended_first_by: Some(kill.target.tid),
        };
        [
            self.access_of(Side {
                op: kill,
                at: AccessPoint::Entry,
            }),
            ended,
        ]
    }

    fn none_found_reason(&self, victims: &Victims<'a>) -> String {
        if let Some(victim) = victims.ranked.first() {
            return format!(
                "{}, but no other thread's access to a resource it touched changes order around \
                 it, and no timed wait or clock read changes order",
                describe(victim.target, victim.baseline)
            );
        }
        let mut reason = format!(
            "no syscall after the deciding event {} returns a different result, bytes or open \
             file in the two runs, and no timed wait or clock read changes order",
            self.decision
        );
        if !victims.memory_diverged.is_empty() {
            let threads: Vec<String> = victims
                .memory_diverged
                .iter()
                .map(|t| format!("thread {t}"))
                .collect();
            reason.push_str(&format!(
                "; {} changed control flow through memory that syscalls do not show",
                threads.join(", ")
            ));
        }
        reason
    }
}

/// The two ends of a happens-before query in one run: the first access's
/// event, and the second's; `None` when either has no event there. With
/// `stretch`, a second access that is ordered by its entry reaches to its
/// return, where a wake or data edge lands (a wait, a read, a killed call);
/// an fd or cursor access happens at its entry.
fn hb_ends(
    sides: [Option<(&SyscallRecord, AccessPoint)>; 2],
    stretch: bool,
) -> Option<(EventRef, EventRef)> {
    let [(a, at_a), (b, at_b)] = [sides[0]?, sides[1]?];
    let from = event_of(a, at_a)?;
    let at = event_of(b, at_b)?;
    let to = match b.post {
        Some(post) if stretch => post.max(at),
        _ => at,
    };
    Some((
        EventRef {
            tid: a.tid,
            event: from,
        },
        EventRef {
            tid: b.tid,
            event: to,
        },
    ))
}

/// Whether `partner` released what the victim reached in the baseline
/// (`used`): it closed the open file the victim reached through the same
/// descriptor slot, or closed the channel end the victim used. That is the
/// access that took it away; a later open or dup onto the number only reuses
/// it.
fn releases_what_the_victim_used(used: &SyscallRecord, partner: &SyscallRecord) -> bool {
    partner.accesses.iter().any(|released| match released {
        Access::Fd {
            slot,
            open_file: Some(file),
            op: FdOp::Close,
        } => used.accesses.iter().any(|access| {
            matches!(access, Access::Fd { slot: s, open_file: Some(f), .. } if s == slot && f == file)
        }),
        Access::Channel {
            open_file,
            op: ChannelOp::Close,
            ..
        } => used.accesses.iter().any(
            |access| matches!(access, Access::Channel { open_file: end, .. } if end == open_file),
        ),
        _ => false,
    })
}

/// Whether a call's outcome differs between the runs: its result, the bytes
/// it returned, or the open file a descriptor reached.
fn differs(target: &SyscallRecord, baseline: &SyscallRecord) -> bool {
    let reached = |sys: &SyscallRecord| -> Vec<(FdSlot, Option<OpenFileId>)> {
        sys.accesses
            .iter()
            .filter_map(|a| match a {
                Access::Fd {
                    slot, open_file, ..
                } => Some((*slot, *open_file)),
                _ => None,
            })
            .collect()
    };
    target.result != baseline.result
        || target.returned != baseline.returned
        || reached(target) != reached(baseline)
}

/// The pair `victim` and `partner` form when they conflict and their order
/// flips, sides in target order.
fn flip<'a>(victim: Op<'a>, partner: Op<'a>) -> Option<Pair<'a>> {
    let victim_baseline = victim.baseline?;
    let conflict = conflict(victim.target, victim_baseline, partner.target)?;
    // Whether the victim's event comes first, when the two events differ.
    let first = |v: Option<&SyscallRecord>, p: Option<&SyscallRecord>, (at_v, at_p)| {
        let (kv, kp) = (key(v, at_v), key(p, at_p));
        (kv != kp).then_some(kv < kp)
    };
    let &(at_v, at_p) = conflict.comparisons.iter().find(|&&points| {
        let in_target = first(Some(victim.target), Some(partner.target), points);
        let in_baseline = first(Some(victim_baseline), partner.baseline, points);
        matches!((in_target, in_baseline), (Some(t), Some(b)) if t != b)
    })?;
    let v = Side {
        op: victim,
        at: at_v,
    };
    let p = Side {
        op: partner,
        at: at_p,
    };
    let (sides, victim) = if p.target_key() < v.target_key() {
        ([p, v], 1)
    } else {
        ([v, p], 0)
    };
    Some(Pair {
        sides,
        victim,
        resource: conflict.resource,
        slot: conflict.slot,
    })
}

/// How the victim conflicts with another call.
struct Conflict {
    resource: ResourceText,
    /// The fd slot, when that is the resource.
    slot: Option<FdSlot>,
    /// Which events of (victim, other) to compare, in order of preference.
    comparisons: &'static [(AccessPoint, AccessPoint)],
}

/// Two calls on one resource compare entries first, then each entry with the
/// other's return (a wake against a wait's timeout). A signal, exit or exec
/// aimed at the victim's thread or process compares with the victim's
/// return, and the other way round.
fn conflict(
    victim: &SyscallRecord,
    victim_baseline: &SyscallRecord,
    other: &SyscallRecord,
) -> Option<Conflict> {
    use AccessPoint::Entry;
    use AccessPoint::Return;
    let shared = victim
        .accesses
        .iter()
        .chain(&victim_baseline.accesses)
        .find(|a| other.accesses.iter().any(|b| a.conflicts_with(b)));
    if let Some(access) = shared {
        return Some(Conflict {
            resource: resource_text(access),
            slot: match access {
                Access::Fd { slot, .. } => Some(*slot),
                _ => None,
            },
            comparisons: &[(Entry, Entry), (Return, Entry), (Entry, Return)],
        });
    }
    let lifetime = |task, comparisons| Conflict {
        resource: task_text(task),
        slot: None,
        comparisons,
    };
    if let Some(task) = ends(other, victim) {
        return Some(lifetime(task, &[(Return, Entry)]));
    }
    ends(victim, other).map(|task| lifetime(task, &[(Entry, Return)]))
}

/// The thread or process of `target` that `killer` signals, ends or
/// replaces. A group kill also lists each member process, so groups match
/// through those entries.
fn ends(killer: &SyscallRecord, target: &SyscallRecord) -> Option<Task> {
    if killer.result.is_some_and(|r| r < 0) {
        // A failed kill, exit or exec ended nothing.
        return None;
    }
    killer.accesses.iter().find_map(|a| match a {
        Access::Lifetime { target: task, op }
            if matches!(op, LifetimeOp::Exit | LifetimeOp::Exec)
                || matches!(op, LifetimeOp::Signal { signo } if *signo != 0) =>
        {
            let hits = match task {
                Task::Thread(tid) => *tid == target.tid,
                Task::Process(pid) => *pid == target.pid,
                Task::ProcessGroup(_) => false,
            };
            hits.then_some(*task)
        }
        _ => None,
    })
}

/// The first pair `(i, j)`, `i < j` in target order, on two threads, whose
/// baseline order is the reverse.
fn first_inversion<'a>(sides: &[Side<'a>]) -> Option<[Side<'a>; 2]> {
    // Per suffix, the smallest baseline key and its thread, and the smallest
    // on any other thread.
    let n = sides.len();
    let mut best = vec![((NEVER, None), NEVER); n + 1];
    for i in (0..n).rev() {
        let (key, tid) = (sides[i].baseline_key(), sides[i].op.target.tid);
        let ((k1, t1), k2) = best[i + 1];
        best[i] = if key < k1 {
            ((key, Some(tid)), if t1 == Some(tid) { k2 } else { k1 })
        } else if t1 != Some(tid) && key < k2 {
            ((k1, t1), key)
        } else {
            ((k1, t1), k2)
        };
    }
    (0..n).find_map(|i| {
        let (key, tid) = (sides[i].baseline_key(), sides[i].op.target.tid);
        let ((k1, t1), k2) = best[i + 1];
        let other = if t1 == Some(tid) { k2 } else { k1 };
        if other >= key {
            return None;
        }
        sides[i + 1..]
            .iter()
            .find(|s| s.op.target.tid != tid && s.baseline_key() < key)
            .map(|&j| [sides[i], j])
    })
}

fn timed_kind(sys: &SyscallRecord) -> Option<Timed> {
    let a = &sys.args;
    let timerfd_read = sys.accesses.iter().any(|x| {
        matches!(
            x,
            Access::Timer {
                timer: TimerId::Fd { .. },
                op: TimerOp::Read
            }
        )
    });
    let wait = match sys.sysno {
        Sysno::nanosleep | Sysno::clock_nanosleep => true,
        Sysno::futex => {
            let cmd = a[1] as i32 & !(libc::FUTEX_PRIVATE_FLAG | libc::FUTEX_CLOCK_REALTIME);
            let waits = [
                libc::FUTEX_WAIT,
                libc::FUTEX_WAIT_BITSET,
                libc::FUTEX_LOCK_PI,
                libc::FUTEX_WAIT_REQUEUE_PI,
                FUTEX_LOCK_PI2,
            ];
            waits.contains(&cmd) && a[3] != 0
        }
        Sysno::epoll_pwait => a[3] as i32 > 0,
        Sysno::ppoll => a[2] != 0,
        Sysno::epoll_pwait2 => a[3] != 0,
        Sysno::pselect6 => a[4] != 0,
        // The legacy calls only x86_64 has; aarch64's generic table
        // (include/uapi/asm-generic/unistd.h) has ppoll, epoll_pwait and
        // pselect6 above instead.
        #[cfg(target_arch = "x86_64")]
        Sysno::poll => a[2] as i32 > 0,
        #[cfg(target_arch = "x86_64")]
        Sysno::epoll_wait => a[3] as i32 > 0,
        #[cfg(target_arch = "x86_64")]
        Sysno::select => a[4] != 0,
        _ => timerfd_read,
    };
    if wait {
        return Some(Timed::Wait);
    }
    reads_a_clock(sys.sysno).then_some(Timed::Clock)
}

/// Whether `sysno` reads a clock: clock_gettime, gettimeofday, and
/// x86_64's time, which aarch64 lacks (its C library builds time(2) on
/// clock_gettime).
fn reads_a_clock(sysno: Sysno) -> bool {
    match sysno {
        Sysno::clock_gettime | Sysno::gettimeofday => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::time => true,
        _ => false,
    }
}

/// `FUTEX_LOCK_PI2` (Linux 5.14), not in every libc release.
const FUTEX_LOCK_PI2: i32 = 13;

/// `thread 7's lseek(6, 2048, SEEK_SET) returns -EBADF in the target and
/// 2048 in the baseline`.
fn describe(target: &SyscallRecord, baseline: Option<&SyscallRecord>) -> String {
    let returns = |sys: &SyscallRecord| match sys.result {
        Some(_) => format!("returns {}", result_text(sys)),
        None => "does not return".to_string(),
    };
    let call = format!("{}'s {}", thread_text(target.tid), call_text(target));
    match baseline {
        Some(b) if b.result == target.result => format!(
            "{call} {} in both runs but reaches different data",
            returns(target)
        ),
        Some(b) if target.result.is_some() && b.result.is_some() => format!(
            "{call} returns {} in the target and {} in the baseline",
            result_text(target),
            result_text(b)
        ),
        Some(b) => format!(
            "{call} {} in the target and {} in the baseline",
            returns(target),
            returns(b)
        ),
        None => format!("{call} {}", returns(target)),
    }
}

fn who(sys: &SyscallRecord) -> String {
    format!("{}'s {}", thread_text(sys.tid), sys.sysno.name())
}

fn thread_text(tid: DetTid) -> String {
    format!("thread {tid}")
}

fn task_text(task: Task) -> ResourceText {
    let text = match task {
        Task::Thread(tid) => format!("thread {tid}"),
        Task::Process(pid) => format!("process {pid}"),
        Task::ProcessGroup(pid) => format!("process group {pid}"),
    };
    ResourceText {
        kind: ResourceKind::Lifetime,
        text,
    }
}

fn resource_text(access: &Access) -> ResourceText {
    let (kind, text) = match access {
        Access::Fd { slot, .. } => (ResourceKind::Fd, slot_text(*slot)),
        Access::OpenFile { open_file, .. } => {
            (ResourceKind::OpenFile, format!("the cursor of {open_file}"))
        }
        Access::FileContents { inode, range, .. } => (
            ResourceKind::FileContents,
            match range {
                Some(r) => format!("bytes {}..{} of inode {inode}", r.start, r.end),
                None => format!("the contents of inode {inode}"),
            },
        ),
        Access::Channel {
            open_file,
            id,
            channel,
            ..
        } => {
            let kind = match channel {
                ChannelKind::Pipe => "pipe",
                ChannelKind::Fifo => "FIFO",
                ChannelKind::StreamSocket => "stream socket",
                ChannelKind::DatagramSocket => "datagram socket",
                ChannelKind::SeqpacketSocket => "seqpacket socket",
                ChannelKind::Socket => "socket",
                ChannelKind::Eventfd => "eventfd",
            };
            let name = match id {
                Some(ChannelId::Pair(first)) => format!("whose first end is {first}"),
                Some(ChannelId::Inode(inode)) => format!("inode {inode}"),
                None => format!("end {open_file}"),
            };
            (ResourceKind::Channel, format!("the {kind} {name}"))
        }
        Access::Futex { futex, .. } => (ResourceKind::Futex, futex_text(futex)),
        Access::Memory { mm, range, .. } => (
            ResourceKind::Memory,
            format!(
                "memory {:#x}..{:#x} of the address space of thread {}",
                range.start,
                range.end,
                mm.creator()
            ),
        ),
        Access::Timer { timer, .. } => (
            ResourceKind::Timer,
            match timer {
                TimerId::Posix { process, id } => format!("timer {id} of process {process}"),
                TimerId::Interval { process, which } => {
                    format!("interval timer {which} of process {process}")
                }
                TimerId::Fd { open_file } => format!("the timerfd {open_file}"),
            },
        ),
        Access::Lifetime { target, .. } => return task_text(*target),
    };
    ResourceText { kind, text }
}

fn slot_text(slot: FdSlot) -> String {
    let files = slot.files;
    match files.generation() {
        0 => format!("fd {} of process {}", slot.fd, files.creator()),
        g => format!(
            "fd {} of process {} (after exec {g})",
            slot.fd,
            files.creator()
        ),
    }
}

fn futex_text(futex: &FutexID) -> String {
    match futex {
        FutexID::Private { mm, address } => format!(
            "the futex word {address:#x} of the address space of thread {}",
            mm.creator()
        ),
        FutexID::Shared { object, offset } => {
            let object = match object {
                SharedMemoryObjectId::Anonymous { origin, sequence } => format!(
                    "shared mapping {sequence} of the address space of thread {}",
                    origin.creator()
                ),
                SharedMemoryObjectId::File { device, inode } => {
                    format!("the file with inode {inode} on device {device}")
                }
                SharedMemoryObjectId::OpenFile { id } => format!("{id}"),
            };
            format!("the process-shared futex word at offset {offset} of {object}")
        }
    }
}

/// What the guest received: the value, `-EBADF`, or `?` for no return.
fn result_text(sys: &SyscallRecord) -> String {
    match sys.result {
        None => "?".to_string(),
        Some(r) if (-4095..0).contains(&r) => match nix::errno::Errno::from_raw(-r as i32) {
            nix::errno::Errno::UnknownErrno => r.to_string(),
            errno => format!("-{errno:?}"),
        },
        Some(r)
            if r > 0xffff
                && matches!(
                    sys.sysno,
                    Sysno::mmap | Sysno::mremap | Sysno::brk | Sysno::shmat
                ) =>
        {
            format!("{r:#x}")
        }
        Some(r) => r.to_string(),
    }
}

/// A syscall with its arguments decoded as far as the record allows.
fn call_text(sys: &SyscallRecord) -> String {
    let a = &sys.args;
    let int = |i: usize| (a[i] as i64).to_string();
    let fd = |i: usize| (a[i] as i32).to_string();
    let ptr = |i: usize| match a[i] {
        0 => "NULL".to_string(),
        p => format!("{p:#x}"),
    };
    let plain = |i: usize| match a[i] {
        n @ 0..=0xffff => n.to_string(),
        n => format!("{n:#x}"),
    };
    let args: Vec<String> = match sys.sysno {
        Sysno::close | Sysno::fsync | Sysno::fdatasync | Sysno::dup | Sysno::syncfs => vec![fd(0)],
        Sysno::exit | Sysno::exit_group => vec![int(0)],
        #[cfg(target_arch = "x86_64")]
        Sysno::dup2 => vec![fd(0), fd(1)],
        Sysno::dup3 => vec![fd(0), fd(1), plain(2)],
        Sysno::write if sys.out_bytes.is_some() => {
            let bytes = sys.out_bytes.as_deref().unwrap_or_default();
            vec![fd(0), quoted(bytes), int(2)]
        }
        Sysno::read | Sysno::write | Sysno::readv | Sysno::writev | Sysno::getdents64 => {
            vec![fd(0), ptr(1), int(2)]
        }
        Sysno::pread64 | Sysno::pwrite64 => vec![fd(0), ptr(1), int(2), int(3)],
        Sysno::lseek => vec![fd(0), int(1), whence_text(a[2])],
        Sysno::fstat => vec![fd(0), ptr(1)],
        Sysno::ftruncate => vec![fd(0), int(1)],
        Sysno::shutdown | Sysno::flock | Sysno::listen => vec![fd(0), plain(1)],
        Sysno::openat => {
            let dirfd = match a[0] as i32 {
                libc::AT_FDCWD => "AT_FDCWD".to_string(),
                d => d.to_string(),
            };
            vec![dirfd, path_text(sys).unwrap_or_else(|| ptr(1)), plain(2)]
        }
        Sysno::futex => {
            let mut args = vec![ptr(0), futex_op_text(a[1]), int(2)];
            let last = (3..6).rev().find(|&i| a[i] != 0).unwrap_or(2);
            args.extend((3..=last).map(ptr));
            args
        }
        Sysno::kill | Sysno::tkill => vec![int(0), signal_text(a[1])],
        Sysno::tgkill => vec![int(0), int(1), signal_text(a[2])],
        Sysno::clock_gettime => vec![clock_text(a[0]), ptr(1)],
        Sysno::clock_nanosleep => vec![clock_text(a[0]), plain(1), ptr(2), ptr(3)],
        Sysno::nanosleep => vec![ptr(0), ptr(1)],
        _ => {
            let last = (0..6).rev().find(|&i| a[i] != 0).unwrap_or(0);
            (0..=last).map(plain).collect()
        }
    };
    format!("{}({})", sys.sysno.name(), args.join(", "))
}

fn path_text(sys: &SyscallRecord) -> Option<String> {
    sys.accesses.iter().find_map(|a| match a {
        Access::Fd {
            op: FdOp::Open { path: Some(path) },
            ..
        } => Some(format!("{path:?}")),
        _ => None,
    })
}

/// The bytes as a quoted string, cut at 48 characters.
fn quoted(bytes: &[u8]) -> String {
    const CAP: usize = 48;
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n');
    let mut shown: String = text.chars().take(CAP).collect();
    if text.chars().count() > CAP {
        shown.push_str("...");
    }
    format!("{shown:?}")
}

fn whence_text(whence: u64) -> String {
    match whence {
        0 => "SEEK_SET".to_string(),
        1 => "SEEK_CUR".to_string(),
        2 => "SEEK_END".to_string(),
        3 => "SEEK_DATA".to_string(),
        4 => "SEEK_HOLE".to_string(),
        w => w.to_string(),
    }
}

fn futex_op_text(op: u64) -> String {
    const NAMES: [&str; 14] = [
        "FUTEX_WAIT",
        "FUTEX_WAKE",
        "FUTEX_FD",
        "FUTEX_REQUEUE",
        "FUTEX_CMP_REQUEUE",
        "FUTEX_WAKE_OP",
        "FUTEX_LOCK_PI",
        "FUTEX_UNLOCK_PI",
        "FUTEX_TRYLOCK_PI",
        "FUTEX_WAIT_BITSET",
        "FUTEX_WAKE_BITSET",
        "FUTEX_WAIT_REQUEUE_PI",
        "FUTEX_CMP_REQUEUE_PI",
        "FUTEX_LOCK_PI2",
    ];
    let op = op as i32;
    let cmd = op & !(libc::FUTEX_PRIVATE_FLAG | libc::FUTEX_CLOCK_REALTIME);
    let Some(name) = usize::try_from(cmd).ok().and_then(|c| NAMES.get(c)) else {
        return op.to_string();
    };
    let mut text = name.to_string();
    if op & libc::FUTEX_PRIVATE_FLAG != 0 {
        text.push_str("_PRIVATE");
    }
    if op & libc::FUTEX_CLOCK_REALTIME != 0 {
        text.push_str("|FUTEX_CLOCK_REALTIME");
    }
    text
}

fn signal_text(signo: u64) -> String {
    match nix::sys::signal::Signal::try_from(signo as i32) {
        Ok(signal) => signal.as_str().to_string(),
        Err(_) => signo.to_string(),
    }
}

fn clock_text(clock: u64) -> String {
    const NAMES: [&str; 8] = [
        "CLOCK_REALTIME",
        "CLOCK_MONOTONIC",
        "CLOCK_PROCESS_CPUTIME_ID",
        "CLOCK_THREAD_CPUTIME_ID",
        "CLOCK_MONOTONIC_RAW",
        "CLOCK_REALTIME_COARSE",
        "CLOCK_MONOTONIC_COARSE",
        "CLOCK_BOOTTIME",
    ];
    usize::try_from(clock)
        .ok()
        .and_then(|c| NAMES.get(c))
        .map_or_else(|| (clock as i64).to_string(), |n| n.to_string())
}

/// The "Racing accesses" block of the human-readable report (design
/// section 8). It never calls a pair a race of its own: the verdict is only
/// whether the edges Hermit observes order it.
pub fn render_racing_accesses(r: &RacingAccesses) -> String {
    let mut lines = Vec::new();
    let end = match &r.outcome {
        Some(o) => format!("the outcome at #{}", o.event),
        None => "the end of the trace".to_string(),
    };
    match r.status {
        RaceStatus::Found => {
            let resource = r.resource.as_ref().map_or("", |x| x.text.as_str());
            lines.push(format!("Racing accesses ({resource}):"));
        }
        RaceStatus::Timing => lines.push(format!(
            "Racing accesses: no conflicting kernel-visible access between event #{} and {end}. \
             The order that flips is the return order of two timed calls:",
            r.decision
        )),
        RaceStatus::NoneFound => lines.push(format!("Racing accesses: none found: {}.", r.reason)),
        RaceStatus::Unavailable => {
            lines.push(format!("Racing accesses: unavailable: {}.", r.reason))
        }
    }
    if let [first, second] = r.accesses.as_slice() {
        lines.extend(access_lines([first, second]));
        lines.push(baseline_sentence(r, [first, second]));
        lines.extend(history_sentences(r, [first, second]));
        if let Some(o) = &r.outcome {
            lines.push(format!(
                "Outcome: {} by thread {} at #{}.",
                o.text, o.thread, o.event
            ));
        }
        if let Some(hb) = &r.happens_before {
            lines.push(happens_before_sentence(hb, [first, second]));
        }
    }
    if !r.also_reordered.is_empty() {
        lines.push("Also reordered:".to_string());
        for pair in &r.also_reordered {
            let [a, b] = &pair.accesses;
            lines.push(format!(
                "  {}: {} ({}) and {} ({})",
                pair.resource.text,
                access_name(a),
                event_text(a.event),
                access_name(b),
                event_text(b.event)
            ));
        }
        if r.more_reordered > 0 {
            lines.push(format!("  and {} more", r.more_reordered));
        }
    }
    lines.extend(r.notes.iter().cloned());
    lines.join("\n")
}

/// `thread 9's close`.
fn access_name(a: &RacingAccess) -> String {
    let name = a.call.split('(').next().unwrap_or(&a.call);
    format!("thread {}'s {name}", a.thread)
}

fn event_text(event: Option<EventIndex>) -> String {
    event.map_or("-".to_string(), |e| format!("#{e}"))
}

/// The two access lines, in columns.
fn access_lines(accesses: [&RacingAccess; 2]) -> Vec<String> {
    let threads = accesses.map(|a| format!("thread {}", a.thread));
    let events = accesses.map(|a| event_text(a.event));
    let tw = threads.iter().map(String::len).max().unwrap_or(0) + 2;
    let ew = events.iter().map(String::len).max().unwrap_or(0) + 2;
    accesses
        .iter()
        .zip(threads.iter().zip(&events))
        .map(|(a, (thread, event))| {
            let returned = match a.at {
                AccessPoint::Entry => "",
                AccessPoint::Return => " (its return)",
            };
            match a.ended_first_by {
                Some(killer) => {
                    let by = accesses
                        .iter()
                        .find(|other| other.thread == killer)
                        .map_or(format!("thread {killer}"), |other| access_name(other));
                    format!(
                        "  {thread:<tw$}{event:<ew$}{} does not run in the target: {by} ended \
                         thread {} first",
                        a.call, a.thread
                    )
                }
                None => format!(
                    "  {thread:<tw$}{event:<ew$}{} = {}{returned}",
                    a.call, a.result
                ),
            }
        })
        .collect()
}

/// What each access did in the baseline, in baseline order.
fn baseline_sentence(r: &RacingAccesses, accesses: [&RacingAccess; 2]) -> String {
    let step = |a: &RacingAccess| {
        let name = access_name(a);
        match (&a.baseline_result, a.baseline_event) {
            (None, _) => match a.baseline_differs_from {
                Some(nth) => format!(
                    "{name} has no matching call (thread {}'s calls differ from its call {nth} \
                     on)",
                    a.thread
                ),
                None => format!("{name} does not run"),
            },
            (Some(_), None) => format!("{name} does not return"),
            (Some(result), Some(e)) => match a.at {
                AccessPoint::Entry if result == "?" => {
                    format!("{name} enters at #{e} and does not return")
                }
                AccessPoint::Entry => format!("{name} at #{e} returns {result}"),
                AccessPoint::Return => format!("{name} returns {result} at #{e}"),
            },
        }
    };
    let order = r.order.as_ref().map_or([0, 1], |o| o.baseline);
    let reversed = if order == [1, 0] {
        " the order is reversed:"
    } else {
        ""
    };
    format!(
        "In the baseline{reversed} {}, and {}.",
        step(accesses[order[0]]),
        step(accesses[order[1]])
    )
}

/// The fd slot's history, and what it implies when a thread closes a
/// number it had already closed.
fn history_sentences(r: &RacingAccesses, accesses: [&RacingAccess; 2]) -> Vec<String> {
    if r.history.is_empty() {
        return Vec::new();
    }
    let items: Vec<String> = r
        .history
        .iter()
        .map(|h| {
            let open_file = h.open_file.as_deref().unwrap_or("an open file");
            let what = match h.kind {
                HistoryKind::Closed => "released the number".to_string(),
                HistoryKind::Installed => format!("installed {open_file}"),
                HistoryKind::LastUse => format!("reached {open_file}"),
            };
            format!("thread {}'s {} {what} (#{})", h.thread, h.call, h.event)
        })
        .collect();
    let mut sentences = vec![format!("Before them in the target: {}.", items.join("; "))];
    let closed = r.history.iter().find(|h| h.kind == HistoryKind::Closed);
    let installed = r.history.iter().find(|h| h.kind == HistoryKind::Installed);
    if let (Some(closed), Some(installed)) = (closed, installed) {
        let again = accesses
            .iter()
            .find(|a| a.thread == closed.thread && a.call.starts_with("close("));
        if let Some(again) =
            again.filter(|_| installed.thread != closed.thread && installed.event > closed.event)
        {
            sentences.push(format!(
                "Thread {} had already closed the number, so its close at {} closes the file \
                 thread {} installed.",
                closed.thread,
                event_text(again.event),
                installed.thread
            ));
        }
    }
    sentences
}

fn happens_before_sentence(hb: &HappensBefore, accesses: [&RacingAccess; 2]) -> String {
    let unordered = "unordered by the edges Hermit observes";
    let is_unordered = |v: &Option<HbVerdict>| v.as_ref().is_some_and(|v| !v.ordered);
    if is_unordered(&hb.target) && (hb.baseline.is_none() || is_unordered(&hb.baseline)) {
        let both = if hb.baseline.is_some() {
            ", in both orders"
        } else {
            " in the target"
        };
        return format!(
            "Happens-before: {unordered}{both}: no edge Hermit records or derives connects {} to \
             {}. {INVISIBLE_SYNC_CAVEAT}",
            access_name(accesses[0]),
            access_name(accesses[1])
        );
    }
    let text = |v: &Option<HbVerdict>| match v {
        None => "not applicable: one of the accesses has no event there".to_string(),
        Some(v) if !v.ordered => unordered.to_string(),
        Some(v) => {
            let chain: Vec<String> = v
                .edges
                .iter()
                .map(|e| {
                    format!(
                        "{} thread {} #{} -> thread {} #{}",
                        edge_kind_text(e.kind),
                        e.from.tid,
                        e.from.event,
                        e.to.tid,
                        e.to.event
                    )
                })
                .collect();
            format!("ordered by {}", chain.join(", then "))
        }
    };
    let caveat = if is_unordered(&hb.target) || is_unordered(&hb.baseline) {
        format!(" {INVISIBLE_SYNC_CAVEAT}")
    } else {
        String::new()
    };
    format!(
        "Happens-before: in the target, {}; in the baseline, {}.{caveat}",
        text(&hb.target),
        text(&hb.baseline)
    )
}

fn edge_kind_text(kind: HbEdgeKind) -> &'static str {
    match kind {
        HbEdgeKind::FutexWake => "futex wake",
        HbEdgeKind::FutexRequeue => "futex requeue",
        HbEdgeKind::Create => "thread creation",
        HbEdgeKind::ExitJoin => "exit to join",
        HbEdgeKind::ChildExit => "child exit to wait",
        HbEdgeKind::Signal => "signal",
        HbEdgeKind::ChannelBytes => "pipe or socket bytes",
        HbEdgeKind::Datagram => "datagram",
        HbEdgeKind::Eventfd => "eventfd count",
    }
}
#[cfg(test)]
mod tests {
    use detcore_model::access_trace::CursorOp;
    use detcore_model::access_trace::EdgeRecord;
    use detcore_model::access_trace::FutexOp;
    use detcore_model::access_trace::ReturnedBytes;
    use detcore_model::access_trace::SyncObject;
    use detcore_model::access_trace::TraceEnd;
    use detcore_model::access_trace::TraceHeader;
    use detcore_model::fd::FilesId;
    use detcore_model::futex::MmId;
    use detcore_model::pid::DetPid;
    use pretty_assertions::assert_eq;

    use super::*;

    /// The deciding event of every scenario but the rendered one.
    const S: EventIndex = 100;
    const EBADF: i64 = -9;
    const EAGAIN: i64 = -11;
    const ETIMEDOUT: i64 = -110;

    fn t(n: i32) -> DetTid {
        DetTid::from_raw(n)
    }

    fn file(creator: i32, sequence: u64) -> OpenFileId {
        OpenFileId::new(t(creator), sequence)
    }

    fn fd(number: i32, open_file: Option<OpenFileId>, op: FdOp) -> Access {
        Access::Fd {
            slot: FdSlot {
                files: FilesId::initial(t(3)),
                fd: number,
            },
            open_file,
            op,
        }
    }

    fn opened(path: &str) -> FdOp {
        FdOp::Open {
            path: Some(path.to_string()),
        }
    }

    fn futex_word() -> FutexID {
        FutexID::private(MmId::initial(t(3)), 0x5000)
    }

    fn futex(op: FutexOp) -> Access {
        Access::Futex {
            futex: futex_word(),
            op,
        }
    }

    fn pipe(open_file: OpenFileId, op: ChannelOp) -> Access {
        Access::Channel {
            open_file,
            id: Some(ChannelId::Pair(file(7, 1))),
            channel: ChannelKind::Pipe,
            op,
        }
    }

    /// A call of thread `tid` (process 3) entering at `pre` and returning
    /// `result` at `pre + 1`; a `None` result never returns.
    fn call(
        pre: EventIndex,
        tid: i32,
        nth: u64,
        sysno: Sysno,
        args: &[u64],
        result: Option<i64>,
        accesses: Vec<Access>,
    ) -> SyscallRecord {
        let mut raw = [0; 6];
        raw[..args.len()].copy_from_slice(args);
        SyscallRecord {
            pre,
            post: result.map(|_| pre + 1),
            tid: t(tid),
            pid: DetPid::from_raw(3),
            nth,
            sysno,
            args: raw,
            result,
            accesses,
            returned: None,
            out_bytes: None,
        }
    }

    fn edge(kind: EdgeKind, from: (i32, EventIndex), to: (i32, EventIndex)) -> TraceRecord {
        TraceRecord::Edge(EdgeRecord {
            kind,
            from: EventRef {
                tid: t(from.0),
                event: from.1,
            },
            to: EventRef {
                tid: t(to.0),
                event: to.1,
            },
            object: SyncObject::Futex(futex_word()),
        })
    }

    /// A complete trace of the calls, in the order the writer emits them.
    fn trace(calls: Vec<SyscallRecord>) -> AccessTrace {
        traced(calls.into_iter().map(TraceRecord::Syscall).collect())
    }

    fn traced(mut records: Vec<TraceRecord>) -> AccessTrace {
        records.sort_by_key(|r| record_span(r).map_or(NEVER, |(_, last)| last));
        AccessTrace {
            header: TraceHeader::new(0),
            end: Some(TraceEnd {
                events: 10_000,
                records: records.len() as u64,
            }),
            records,
        }
    }

    fn find(
        target: &AccessTrace,
        baseline: &AccessTrace,
        outcome_thread: Option<i32>,
    ) -> RacingAccesses {
        let window = Window {
            decision: S,
            outcome: None,
        };
        find_racing_accesses(target, baseline, window, outcome_thread.map(t))
    }

    fn access(
        thread: i32,
        nth: u64,
        at: AccessPoint,
        events: (Option<EventIndex>, Option<EventIndex>),
        call: &str,
        results: (&str, Option<&str>),
    ) -> RacingAccess {
        RacingAccess {
            thread: t(thread),
            nth,
            at,
            event: events.0,
            baseline_event: events.1,
            call: call.to_string(),
            result: results.0.to_string(),
            baseline_result: results.1.map(str::to_string),
            baseline_differs_from: None,
            ended_first_by: None,
        }
    }

    fn unordered() -> Option<HbVerdict> {
        Some(HbVerdict {
            ordered: false,
            edges: Vec::new(),
        })
    }

    fn close6(
        pre: EventIndex,
        tid: i32,
        nth: u64,
        open_file: Option<OpenFileId>,
        result: i64,
    ) -> SyscallRecord {
        call(
            pre,
            tid,
            nth,
            Sysno::close,
            &[6],
            Some(result),
            vec![fd(6, open_file, FdOp::Close)],
        )
    }

    /// Thread 7 opens fd 6 before the window; with the deciding event,
    /// thread 9 closes it before thread 7 reads it.
    fn close_then_read() -> (AccessTrace, AccessTrace) {
        let mine = Some(file(7, 1));
        let open = call(
            90,
            7,
            19,
            Sysno::openat,
            &[-100i64 as u64, 0x2000, 0],
            Some(6),
            vec![fd(6, mine, opened("/tmp/a"))],
        );
        let read = |pre, result, accesses| {
            call(
                pre,
                7,
                20,
                Sysno::read,
                &[6, 0x1000, 64],
                Some(result),
                accesses,
            )
        };
        let target = trace(vec![
            open.clone(),
            close6(101, 9, 10, mine, 0),
            read(103, EBADF, vec![fd(6, None, FdOp::Use)]),
        ]);
        let baseline = trace(vec![
            open,
            read(
                101,
                64,
                vec![
                    fd(6, mine, FdOp::Use),
                    Access::OpenFile {
                        open_file: file(7, 1),
                        op: CursorOp::Read,
                    },
                ],
            ),
            close6(103, 9, 10, mine, 0),
        ]);
        (target, baseline)
    }

    #[test]
    fn close_against_a_use_that_fails_with_ebadf() {
        let (target, baseline) = close_then_read();
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.resource,
            Some(ResourceText {
                kind: ResourceKind::Fd,
                text: "fd 6 of process 3".to_string()
            })
        );
        assert_eq!(
            r.accesses,
            vec![
                access(
                    9,
                    10,
                    AccessPoint::Entry,
                    (Some(101), Some(103)),
                    "close(6)",
                    ("0", Some("0"))
                ),
                access(
                    7,
                    20,
                    AccessPoint::Entry,
                    (Some(103), Some(101)),
                    "read(6, 0x1000, 64)",
                    ("-EBADF", Some("64"))
                ),
            ]
        );
        assert_eq!(r.order.unwrap().baseline, [1, 0]);
        let hb = r.happens_before.unwrap();
        assert_eq!((hb.target, hb.baseline), (unordered(), unordered()));
        assert!(
            r.reason.starts_with(
                "thread 7's read(6, 0x1000, 64) returns -EBADF in the target and 64 in the baseline; thread 9's close on fd 6 of process 3 runs before it"
            ),
            "{}",
            r.reason
        );
        assert!(!r.truncated);
    }

    #[test]
    fn stale_fd_after_reuse_reaches_another_open_file() {
        // Thread 5 opened fd 5 and read it before the window. With the
        // deciding event, thread 6 closes fd 5 and opens another file at
        // the number before thread 5 reads it again. Both files hold the
        // same bytes, so only the open file differs.
        let (old, new) = (Some(file(5, 1)), Some(file(6, 1)));
        let same_bytes = |mut sys: SyscallRecord| {
            sys.returned = Some(ReturnedBytes {
                len: 4,
                digest: 7,
                truncated: false,
            });
            sys
        };
        let prefix = vec![
            call(
                10,
                5,
                1,
                Sysno::openat,
                &[-100i64 as u64, 0x2000, 0],
                Some(5),
                vec![fd(5, old, opened("/tmp/old"))],
            ),
            same_bytes(call(
                12,
                5,
                2,
                Sysno::read,
                &[5, 0x1000, 4],
                Some(4),
                vec![fd(5, old, FdOp::Use)],
            )),
        ];
        let close = |pre| {
            call(
                pre,
                6,
                7,
                Sysno::close,
                &[5],
                Some(0),
                vec![fd(5, old, FdOp::Close)],
            )
        };
        let reopen = |pre| {
            call(
                pre,
                6,
                8,
                Sysno::openat,
                &[-100i64 as u64, 0x3000, 0],
                Some(5),
                vec![fd(5, new, opened("/tmp/new"))],
            )
        };
        let reread = |pre, open_file| {
            same_bytes(call(
                pre,
                5,
                3,
                Sysno::read,
                &[5, 0x1000, 4],
                Some(4),
                vec![fd(5, open_file, FdOp::Use)],
            ))
        };
        let target = trace(
            [
                prefix.clone(),
                vec![close(101), reopen(103), reread(105, new)],
            ]
            .concat(),
        );
        let baseline = trace([prefix, vec![reread(101, old), close(103), reopen(105)]].concat());

        let r = find(&target, &baseline, Some(5));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| (a.thread, a.call.as_str()))
                .collect::<Vec<_>>(),
            vec![(t(6), "close(5)"), (t(5), "read(5, 0x1000, 4)")],
            "the close took thread 5's file away; the reopen only reused the number"
        );
        assert_eq!(
            r.history
                .iter()
                .map(|h| (h.kind, h.thread, h.event))
                .collect::<Vec<_>>(),
            vec![
                (HistoryKind::Installed, t(5), 10),
                (HistoryKind::LastUse, t(5), 12),
            ]
        );
        assert_eq!(
            r.history[1].open_file.as_deref(),
            Some("file 1 opened by thread 5")
        );
        assert_eq!(
            r.also_reordered[0].accesses[0].call, "openat(AT_FDCWD, \"/tmp/new\", 0)",
            "the reopen is flipped too"
        );
    }

    #[test]
    fn double_close_pairs_the_two_closes() {
        let shared = Some(file(9, 1));
        let target = trace(vec![
            close6(101, 9, 10, shared, 0),
            close6(103, 7, 20, None, EBADF),
        ]);
        let baseline = trace(vec![
            close6(101, 7, 20, shared, 0),
            close6(103, 9, 10, None, EBADF),
        ]);
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert!(
            r.reason.starts_with("thread 7's close(6) returns -EBADF"),
            "the outcome thread's difference is the victim: {}",
            r.reason
        );
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| (a.thread, a.result.as_str(), a.baseline_result.as_deref()))
                .collect::<Vec<_>>(),
            vec![(t(9), "0", Some("-EBADF")), (t(7), "-EBADF", Some("0"))]
        );
    }

    #[test]
    fn lost_futex_wake_pairs_the_wake_with_the_wait() {
        // With the deciding event, thread 7 wakes nobody before thread 9
        // waits, and thread 9 never returns.
        let wake = |pre, woke| {
            call(
                pre,
                7,
                30,
                Sysno::futex,
                &[0x5000, 129, 1],
                Some(woke),
                vec![futex(FutexOp::Wake)],
            )
        };
        let wait = |pre, result| {
            call(
                pre,
                9,
                40,
                Sysno::futex,
                &[0x5000, 128, 0],
                result,
                vec![futex(FutexOp::Wait)],
            )
        };
        let target = trace(vec![wake(101, 0), wait(103, None)]);
        let mut woken = wait(101, Some(0));
        woken.post = Some(104);
        let baseline = traced(vec![
            TraceRecord::Syscall(woken),
            TraceRecord::Syscall(wake(102, 1)),
            edge(EdgeKind::FutexWake, (7, 102), (9, 104)),
        ]);
        let r = find(&target, &baseline, None);
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.resource.as_ref().unwrap().kind, ResourceKind::Futex);
        assert_eq!(
            r.accesses,
            vec![
                access(
                    7,
                    30,
                    AccessPoint::Entry,
                    (Some(101), Some(102)),
                    "futex(0x5000, FUTEX_WAKE_PRIVATE, 1)",
                    ("0", Some("1"))
                ),
                access(
                    9,
                    40,
                    AccessPoint::Entry,
                    (Some(103), Some(101)),
                    "futex(0x5000, FUTEX_WAIT_PRIVATE, 0)",
                    ("?", Some("0"))
                ),
            ]
        );
        let hb = r.happens_before.unwrap();
        assert_eq!((hb.target, hb.baseline), (unordered(), unordered()));
    }

    #[test]
    fn kill_against_a_futex_return_is_a_lifetime_conflict() {
        // Thread 5 of process 5 waits with a timeout from before the window.
        // With the deciding event it times out before thread 3 kills its
        // process; without it, the kill lands while it waits.
        let wait = |post: Option<EventIndex>| {
            let mut sys = call(
                95,
                5,
                50,
                Sysno::futex,
                &[0x6000, 137, 0, 0x7000, 0, 0xffff_ffff],
                post.map(|_| ETIMEDOUT),
                vec![futex(FutexOp::Wait)],
            );
            sys.pid = DetPid::from_raw(5);
            sys.post = post;
            sys
        };
        let kill_returning = |pre, result| {
            call(
                pre,
                3,
                60,
                Sysno::kill,
                &[5, 9],
                Some(result),
                vec![Access::Lifetime {
                    target: Task::Process(DetPid::from_raw(5)),
                    op: LifetimeOp::Signal { signo: 9 },
                }],
            )
        };
        let kill = |pre| kill_returning(pre, 0);
        // A kill that failed (EPERM) ended nothing, so it races nothing.
        let failed = find(
            &trace(vec![wait(Some(101)), kill_returning(103, -1)]),
            &trace(vec![wait(None), kill_returning(101, -1)]),
            Some(3),
        );
        assert_ne!(failed.status, RaceStatus::Found, "{}", failed.reason);
        let target = trace(vec![wait(Some(101)), kill(103)]);
        let baseline = trace(vec![wait(None), kill(101)]);
        let r = find(&target, &baseline, Some(3));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.resource,
            Some(ResourceText {
                kind: ResourceKind::Lifetime,
                text: "process 5".to_string()
            })
        );
        assert_eq!(
            r.accesses,
            vec![
                access(
                    5,
                    50,
                    AccessPoint::Return,
                    (Some(101), None),
                    "futex(0x6000, FUTEX_WAIT_BITSET_PRIVATE, 0, 0x7000, NULL, 0xffffffff)",
                    ("-ETIMEDOUT", Some("?"))
                ),
                access(
                    3,
                    60,
                    AccessPoint::Entry,
                    (Some(103), Some(101)),
                    "kill(5, SIGKILL)",
                    ("0", Some("0"))
                ),
            ]
        );
        let hb = r.happens_before.unwrap();
        assert_eq!(hb.target, unordered());
        assert_eq!(hb.baseline, None, "the wait never returns in the baseline");
    }

    #[test]
    fn pipe_bytes_order_the_write_before_the_read() {
        let write = |pre, tid, nth| {
            call(
                pre,
                tid,
                nth,
                Sysno::write,
                &[4, 0x1000, 4],
                Some(4),
                vec![pipe(file(7, 2), ChannelOp::Write)],
            )
        };
        let read = |pre, nth, result| {
            call(
                pre,
                9,
                nth,
                Sysno::read,
                &[3, 0x2000, 4],
                Some(result),
                vec![pipe(file(7, 1), ChannelOp::Read)],
            )
        };

        // Each read is reached from the write that put its bytes in: offsets
        // per channel, not every earlier write.
        let calls = [
            write(10, 7, 1),
            write(12, 8, 1),
            read(14, 1, 4),
            read(16, 2, 4),
        ];
        let edges = data_edges(&calls.iter().collect::<Vec<_>>()).edges;
        let channel = |from: (i32, EventIndex), to: (i32, EventIndex)| HbEdge {
            from: EventRef {
                tid: t(from.0),
                event: from.1,
            },
            to: EventRef {
                tid: t(to.0),
                event: to.1,
            },
            kind: HbEdgeKind::ChannelBytes,
        };
        assert_eq!(
            edges,
            vec![channel((7, 10), (9, 15)), channel((8, 12), (9, 17))]
        );

        // A socketpair carries one stream each way: a read on one end takes
        // what the other end wrote.
        let end = |open_file, op| Access::Channel {
            open_file,
            id: Some(ChannelId::Pair(file(7, 5))),
            channel: ChannelKind::StreamSocket,
            op,
        };
        let (a, b) = (file(7, 5), file(7, 6));
        let calls = [
            call(
                10,
                7,
                1,
                Sysno::write,
                &[5, 0x1000, 4],
                Some(4),
                vec![end(a, ChannelOp::Write)],
            ),
            call(
                12,
                9,
                1,
                Sysno::write,
                &[6, 0x1000, 4],
                Some(4),
                vec![end(b, ChannelOp::Write)],
            ),
            call(
                14,
                7,
                2,
                Sysno::read,
                &[5, 0x2000, 4],
                Some(4),
                vec![end(a, ChannelOp::Read)],
            ),
            call(
                16,
                9,
                2,
                Sysno::read,
                &[6, 0x2000, 4],
                Some(4),
                vec![end(b, ChannelOp::Read)],
            ),
        ];
        assert_eq!(
            data_edges(&calls.iter().collect::<Vec<_>>()).edges,
            vec![channel((9, 12), (7, 15)), channel((7, 10), (9, 17))]
        );

        // In semaphore mode one eventfd write feeds two reads; the second
        // read, with nothing new, is reached from the last write.
        let counter = |open_file, op| Access::Channel {
            open_file,
            id: Some(ChannelId::Pair(file(7, 8))),
            channel: ChannelKind::Eventfd,
            op,
        };
        let calls = [
            call(
                10,
                7,
                1,
                Sysno::write,
                &[8, 0x1000, 8],
                Some(8),
                vec![counter(file(7, 8), ChannelOp::Write)],
            ),
            call(
                12,
                9,
                1,
                Sysno::read,
                &[8, 0x2000, 8],
                Some(8),
                vec![counter(file(7, 8), ChannelOp::Read)],
            ),
            call(
                14,
                9,
                2,
                Sysno::read,
                &[8, 0x2000, 8],
                Some(8),
                vec![counter(file(7, 8), ChannelOp::Read)],
            ),
        ];
        let data = data_edges(&calls.iter().collect::<Vec<_>>());
        let count = |to| HbEdge {
            kind: HbEdgeKind::Eventfd,
            ..channel((7, 10), (9, to))
        };
        assert_eq!(data.edges, vec![count(13), count(15)]);
        assert!(data.untracked.is_empty(), "{:?}", data.untracked);

        // With the deciding event the write comes first and the read returns
        // its bytes; without it the read finds the pipe empty.
        let target = trace(vec![write(101, 7, 30), read(103, 40, 4)]);
        let baseline = trace(vec![read(101, 40, EAGAIN), write(103, 7, 30)]);
        let r = find(&target, &baseline, Some(9));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        let hb = r.happens_before.clone().unwrap();
        assert_eq!(
            hb.target,
            Some(HbVerdict {
                ordered: true,
                edges: vec![channel((7, 101), (9, 104))],
            })
        );
        assert_eq!(hb.baseline, unordered());
        // The traces do not show the pipe being created, so bytes queued
        // before them could be credited to the traced write.
        assert_eq!(
            r.notes,
            vec!["Data through the pipe whose first end is file 1 opened by thread 7 may include bytes queued before the trace starts, which the trace does not show being created: a read of them is credited to a later write.".to_string()]
        );
        let created = call(
            90,
            7,
            29,
            Sysno::pipe2,
            &[0x3000, 0],
            Some(0),
            vec![
                fd(3, Some(file(7, 1)), opened("pipe")),
                fd(4, Some(file(7, 2)), opened("pipe")),
            ],
        );
        let target = trace(vec![created.clone(), write(101, 7, 30), read(103, 40, 4)]);
        let baseline = trace(vec![created, read(101, 40, EAGAIN), write(103, 7, 30)]);
        assert_eq!(
            find(&target, &baseline, Some(9)).notes,
            Vec::<String>::new()
        );
    }

    #[test]
    fn futex_wake_edge_orders_a_pair_on_another_resource() {
        // Thread 7 closes fd 6 and wakes thread 9, which then uses fd 6.
        let mine = Some(file(7, 1));
        let fstat = |pre, open_file, result| {
            call(
                pre,
                9,
                41,
                Sysno::fstat,
                &[6, 0x3000],
                Some(result),
                vec![fd(6, open_file, FdOp::Use)],
            )
        };
        let wake = |pre| {
            call(
                pre,
                7,
                31,
                Sysno::futex,
                &[0x5000, 129, 1],
                Some(1),
                vec![futex(FutexOp::Wake)],
            )
        };
        let wait = |post| {
            let mut sys = call(
                98,
                9,
                40,
                Sysno::futex,
                &[0x5000, 128, 0],
                Some(0),
                vec![futex(FutexOp::Wait)],
            );
            sys.post = Some(post);
            sys
        };
        let target = traced(vec![
            TraceRecord::Syscall(close6(101, 7, 30, mine, 0)),
            TraceRecord::Syscall(wake(103)),
            TraceRecord::Syscall(wait(105)),
            edge(EdgeKind::FutexWake, (7, 103), (9, 105)),
            TraceRecord::Syscall(fstat(107, None, EBADF)),
        ]);
        let baseline = trace(vec![
            wait(101),
            fstat(102, mine, 0),
            close6(104, 7, 30, mine, 0),
            wake(106),
        ]);
        let r = find(&target, &baseline, Some(9));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| a.call.as_str())
                .collect::<Vec<_>>(),
            vec!["close(6)", "fstat(6, 0x3000)"]
        );
        let hb = r.happens_before.clone().unwrap();
        let wake_edge = HbEdge {
            from: EventRef {
                tid: t(7),
                event: 103,
            },
            to: EventRef {
                tid: t(9),
                event: 105,
            },
            kind: HbEdgeKind::FutexWake,
        };
        assert_eq!(
            hb.target,
            Some(HbVerdict {
                ordered: true,
                edges: vec![wake_edge],
            })
        );
        assert_eq!(hb.baseline, unordered());
        let text = render_racing_accesses(&r);
        assert!(
            text.contains("Happens-before: in the target, ordered by futex wake thread 7 #103 -> thread 9 #105; in the baseline, unordered by the edges Hermit observes. Uncontended locks and atomics are not visible to Hermit."),
            "{text}"
        );
    }

    #[test]
    fn timed_waits_whose_returns_flip_make_a_timing_pair() {
        // No result differs. Two clock reads flip first, but timed waits are
        // preferred.
        let sleep = |tid, nth, post| {
            let mut sys = call(
                90 + tid as u64,
                tid,
                nth,
                Sysno::clock_nanosleep,
                &[1, 0, 0x1000, 0],
                Some(0),
                Vec::new(),
            );
            sys.post = Some(post);
            sys
        };
        let clock = |pre, tid| {
            call(
                pre,
                tid,
                1,
                Sysno::clock_gettime,
                &[1, 0x2000],
                Some(0),
                Vec::new(),
            )
        };
        let target = trace(vec![
            clock(100, 7),
            clock(102, 8),
            sleep(9, 1, 105),
            sleep(5, 1, 107),
        ]);
        let baseline = trace(vec![
            clock(100, 8),
            clock(102, 7),
            sleep(5, 1, 105),
            sleep(9, 1, 107),
        ]);
        let r = find(&target, &baseline, None);
        assert_eq!(r.status, RaceStatus::Timing, "{}", r.reason);
        assert_eq!(r.resource, None);
        assert_eq!(
            r.accesses,
            vec![
                access(
                    9,
                    1,
                    AccessPoint::Return,
                    (Some(105), Some(107)),
                    "clock_nanosleep(CLOCK_MONOTONIC, 0, 0x1000, NULL)",
                    ("0", Some("0"))
                ),
                access(
                    5,
                    1,
                    AccessPoint::Return,
                    (Some(107), Some(105)),
                    "clock_nanosleep(CLOCK_MONOTONIC, 0, 0x1000, NULL)",
                    ("0", Some("0"))
                ),
            ]
        );
        assert_eq!(r.order.unwrap().baseline, [1, 0]);
    }

    #[test]
    fn a_cut_short_trace_marks_the_window_truncated() {
        let (target, baseline) = close_then_read();
        assert!(!find(&target, &baseline, Some(7)).truncated);

        // The baseline stops after thread 7's read: thread 9's close is
        // missing, which counts as running later, and the result says the
        // trace was cut.
        let mut cut = baseline.clone();
        cut.records.pop();
        cut.end = None;
        let r = find(&target, &cut, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.accesses[0].baseline_result, None);
        assert!(r.truncated);
        assert_eq!(
            r.notes,
            vec!["The baseline trace ends at event 102 without an end line: a call missing from it may have run later.".to_string()]
        );

        // A target cut before the outcome it should reach.
        let mut cut = target.clone();
        cut.end = None;
        let window = Window {
            decision: S,
            outcome: Some(OutcomeRef {
                event: 200,
                thread: t(7),
                text: "exit_group(1)".to_string(),
            }),
        };
        let r = find_racing_accesses(&cut, &baseline, window.clone(), Some(t(7)));
        assert!(r.truncated);
        assert!(r.notes[0].starts_with("The target trace ends at event 104 without an end line, before the outcome at event 200"), "{:?}", r.notes);
        // The same cut trace covers an earlier outcome.
        let earlier = Window {
            outcome: Some(OutcomeRef {
                event: 104,
                ..window.outcome.unwrap()
            }),
            ..window
        };
        assert!(!find_racing_accesses(&cut, &baseline, earlier, Some(t(7))).truncated);
    }

    #[test]
    fn none_found_says_why() {
        // Thread 7 reads different bytes from a file no other thread uses.
        let read = |digest| {
            let mut sys = call(
                101,
                7,
                20,
                Sysno::pread64,
                &[6, 0x1000, 4, 0],
                Some(4),
                vec![fd(6, Some(file(7, 1)), FdOp::Use)],
            );
            sys.returned = Some(ReturnedBytes {
                len: 4,
                digest,
                truncated: false,
            });
            sys
        };
        let other = call(103, 9, 10, Sysno::getpid, &[], Some(3), Vec::new());
        let r = find(
            &trace(vec![read(1), other.clone()]),
            &trace(vec![read(2), other.clone()]),
            Some(7),
        );
        assert_eq!(r.status, RaceStatus::NoneFound);
        assert_eq!(
            r.reason,
            "thread 7's pread64(6, 0x1000, 4, 0) returns 4 in both runs but reaches different data, but no other thread's access to a resource it touched changes order around it, and no timed wait or clock read changes order"
        );

        // No result differs, but thread 7 then makes a different call.
        let write = |fd_number| {
            call(
                105,
                7,
                21,
                Sysno::write,
                &[fd_number, 0x1000, 4],
                Some(4),
                Vec::new(),
            )
        };
        let r = find(
            &trace(vec![other.clone(), write(1)]),
            &trace(vec![other, write(2)]),
            Some(7),
        );
        assert_eq!(r.status, RaceStatus::NoneFound);
        assert!(
            r.reason.ends_with(
                "; thread 7 changed control flow through memory that syscalls do not show"
            ),
            "{}",
            r.reason
        );
        assert!(render_racing_accesses(&r).starts_with(
            "Racing accesses: none found: no syscall after the deciding event 100 returns"
        ));
    }

    #[test]
    fn also_reordered_lists_three_and_counts_the_rest() {
        // Five threads read fd 6 through its cursor and thread 9 closes it,
        // all before thread 7's lseek with the deciding event and after it
        // without.
        let mine = Some(file(7, 1));
        let cursor = Access::OpenFile {
            open_file: file(7, 1),
            op: CursorOp::Read,
        };
        let reader = |pre, tid| {
            call(
                pre,
                tid,
                1,
                Sysno::read,
                &[6, 0x1000, 4],
                Some(4),
                vec![fd(6, mine, FdOp::Use), cursor.clone()],
            )
        };
        let lseek = |pre, result, accesses| {
            call(pre, 7, 20, Sysno::lseek, &[6, 0, 0], Some(result), accesses)
        };
        let mut target: Vec<SyscallRecord> =
            (0..5).map(|i| reader(101 + 2 * i, 10 + i as i32)).collect();
        target.push(close6(111, 9, 10, mine, 0));
        target.push(lseek(113, EBADF, vec![fd(6, None, FdOp::Use)]));
        let mut baseline = vec![lseek(
            101,
            0,
            vec![
                fd(6, mine, FdOp::Use),
                Access::OpenFile {
                    open_file: file(7, 1),
                    op: CursorOp::Seek,
                },
            ],
        )];
        baseline.extend((0..5).map(|i| reader(103 + 2 * i, 10 + i as i32)));
        baseline.push(close6(113, 9, 10, mine, 0));
        let r = find(&trace(target), &trace(baseline), Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.accesses[0].thread,
            t(9),
            "the latest conflicting call before the victim"
        );
        assert_eq!(
            r.also_reordered
                .iter()
                .map(|p| p.accesses[0].thread)
                .collect::<Vec<_>>(),
            vec![t(14), t(13), t(12)]
        );
        assert_eq!(
            r.also_reordered[0].resource.text,
            "the cursor of file 1 opened by thread 7"
        );
        assert_eq!(r.more_reordered, 2);
        let text = render_racing_accesses(&r);
        assert!(text.ends_with("  the cursor of file 1 opened by thread 7: thread 12's read (#105) and thread 7's lseek (#113)\n  and 2 more"), "{text}");
    }

    #[test]
    fn failed_calls_stay_out_of_the_fd_history() {
        // Thread 8's close(6) fails with EBADF between thread 9's real
        // close and thread 7's reopen; it released nothing.
        let failed = fdsan_with(vec![close6(7560, 8, 30, None, EBADF)]);
        // A close interrupted after it released the number (EINTR) did
        // release it.
        let interrupted = fdsan_with(vec![close6(7560, 8, 30, Some(file(9, 4)), -4)]);
        assert_eq!(
            interrupted
                .history
                .first()
                .map(|h| (h.kind, h.thread, h.event)),
            Some((HistoryKind::Closed, t(8), 7560))
        );
        let r = failed;
        assert_eq!(
            r.history
                .iter()
                .map(|h| (h.kind, h.thread, h.event))
                .collect::<Vec<_>>(),
            vec![
                (HistoryKind::Closed, t(9), 7552),
                (HistoryKind::Installed, t(7), 7571),
            ]
        );
    }

    #[test]
    fn the_close_that_released_the_victims_file_outranks_a_later_dup() {
        // Thread 7 opened fd 6 before the window. With the deciding event,
        // thread 9 closes it (a deferred second close), then dups its pipe
        // onto the number, and thread 7's lseek reaches the pipe (ESPIPE).
        // The dup is the latest conflicting call before the lseek, but the
        // close took thread 7's file away; the dup only reused the number.
        let (mine, pipe_end) = (Some(file(7, 1)), Some(file(9, 2)));
        let open = call(
            90,
            7,
            19,
            Sysno::openat,
            &[-100i64 as u64, 0x2000, 2],
            Some(6),
            vec![fd(6, mine, opened("/tmp/misc"))],
        );
        let dup = |pre| {
            call(
                pre,
                9,
                11,
                Sysno::dup3,
                &[4, 6],
                Some(6),
                vec![
                    fd(4, pipe_end, FdOp::Use),
                    fd(6, pipe_end, FdOp::Dup { source: 4 }),
                ],
            )
        };
        let lseek = |pre, result, open_file| {
            call(
                pre,
                7,
                20,
                Sysno::lseek,
                &[6, 2048, 0],
                Some(result),
                vec![fd(6, open_file, FdOp::Use)],
            )
        };
        let target = trace(vec![
            open.clone(),
            close6(101, 9, 10, mine, 0),
            dup(103),
            lseek(105, -29, pipe_end),
        ]);
        let baseline = trace(vec![
            open,
            lseek(101, 2048, mine),
            close6(103, 9, 10, mine, 0),
            dup(105),
        ]);
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| a.call.as_str())
                .collect::<Vec<_>>(),
            vec!["close(6)", "lseek(6, 2048, SEEK_SET)"]
        );
        assert_eq!(
            r.also_reordered[0].accesses[0].call, "dup3(4, 6, 0)",
            "the dup is listed after it"
        );
    }

    #[test]
    fn calls_before_the_decision_align_whatever_their_arguments() {
        // The main thread's first call, the container's execve, carries
        // Hermit's launcher pointers, which differ between the runs. It
        // entered before the decision, so it splits nothing: the thread's
        // later calls still align, and its read is the victim.
        let (mut target, mut baseline) = close_then_read();
        let exec = |argv: u64| {
            let mut sys = call(
                1,
                7,
                1,
                Sysno::execve,
                &[argv, 0x5000, 0x6000],
                None,
                Vec::new(),
            );
            sys.post = None;
            TraceRecord::Syscall(sys)
        };
        target.records.insert(0, exec(0x5555_0000_1000));
        baseline.records.insert(0, exec(0x5555_0000_2000));
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.accesses[1].call, "read(6, 0x1000, 64)");
    }

    #[test]
    fn a_kill_that_ends_a_thread_before_its_next_call_is_a_lifetime_pair() {
        // The w13b pattern. Thread 5 (process 5) holds a process-shared
        // mutex after an uncontended relock. With the deciding event, thread
        // 3 kills process 5 before thread 5's next cond wait, which would
        // release the mutex, so thread 7's wait for the mutex never returns.
        // In the baseline the cond wait enters first.
        let mutex = Access::Futex {
            futex: futex_word(),
            op: FutexOp::Wait,
        };
        let app = |mut sys: SyscallRecord| {
            sys.pid = DetPid::from_raw(5);
            sys
        };
        let clock = app(call(
            101,
            5,
            10,
            Sysno::clock_gettime,
            &[1, 0x2000],
            Some(0),
            Vec::new(),
        ));
        let mut cond_wait = app(call(
            103,
            5,
            11,
            Sysno::futex,
            &[0x6000, 137, 0, 0x7000],
            None,
            vec![futex(FutexOp::Wait)],
        ));
        cond_wait.post = None;
        let kill_with = |pre, signo: i32, result| {
            call(
                pre,
                3,
                20,
                Sysno::kill,
                &[5, signo as u64],
                Some(result),
                vec![Access::Lifetime {
                    target: Task::Process(DetPid::from_raw(5)),
                    op: LifetimeOp::Signal { signo },
                }],
            )
        };
        let kill = |pre| kill_with(pre, 9, 0);
        let lock = |result: Option<i64>| {
            let mut sys = call(
                108,
                7,
                30,
                Sysno::futex,
                &[0x5000, 128, 2],
                result,
                vec![mutex.clone()],
            );
            sys.post = result.map(|_| 112);
            sys
        };
        let target = trace(vec![clock.clone(), kill(103), lock(None)]);
        let baseline = trace(vec![
            clock.clone(),
            cond_wait.clone(),
            kill(105),
            lock(Some(0)),
        ]);

        // A kill that failed, or a signal the trace does not show ending
        // the process (a handled SIGUSR1), ended nothing.
        for (signo, result) in [(9, -1), (libc::SIGUSR1, 0)] {
            let target = trace(vec![
                clock.clone(),
                kill_with(103, signo, result),
                lock(None),
            ]);
            let baseline = trace(vec![
                clock.clone(),
                cond_wait.clone(),
                kill_with(105, signo, result),
                lock(Some(0)),
            ]);
            let r = find(&target, &baseline, Some(3));
            assert_ne!(
                r.status,
                RaceStatus::Found,
                "signal {signo} = {result}: {}",
                r.reason
            );
        }

        // Two timed sleeps that flip: when neither the killer nor the ended
        // thread produced the outcome, the timing pair comes first.
        let sleep = |tid, post| {
            let mut sys = call(
                90 + tid as u64,
                tid,
                1,
                Sysno::nanosleep,
                &[0x1000, 0],
                Some(0),
                Vec::new(),
            );
            sys.post = Some(post);
            TraceRecord::Syscall(sys)
        };
        let with_sleeps = |trace: &AccessTrace, posts: [EventIndex; 2]| {
            let mut records = trace.records.clone();
            records.push(sleep(8, posts[0]));
            records.push(sleep(9, posts[1]));
            traced(records)
        };
        let (timed_target, timed_baseline) = (
            with_sleeps(&target, [110, 111]),
            with_sleeps(&baseline, [111, 110]),
        );
        assert_eq!(
            find(&timed_target, &timed_baseline, Some(7)).status,
            RaceStatus::Timing
        );
        assert_eq!(
            find(&timed_target, &timed_baseline, Some(3)).status,
            RaceStatus::Found
        );

        let r = find(&target, &baseline, Some(3));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.resource.as_ref().map(|x| x.text.as_str()),
            Some("process 5")
        );
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| (a.thread, a.event, a.baseline_event, a.ended_first_by))
                .collect::<Vec<_>>(),
            vec![
                (t(3), Some(103), Some(105), None),
                (t(5), None, Some(103), Some(t(3)))
            ]
        );
        assert_eq!(r.order.as_ref().unwrap().baseline, [1, 0]);
        let hb = r.happens_before.clone().unwrap();
        assert_eq!((hb.target, hb.baseline), (None, unordered()));
        let text = render_racing_accesses(&r);
        assert!(
            text.contains("futex(0x6000, FUTEX_WAIT_BITSET_PRIVATE, 0, 0x7000) does not run in the target: thread 3's kill ended thread 5 first"),
            "{text}"
        );
        assert!(
            text.contains("Happens-before: in the target, not applicable: one of the accesses has no event there; in the baseline, unordered by the edges Hermit observes."),
            "{text}"
        );
    }

    #[test]
    fn a_wait_entered_before_the_decision_aligns_whatever_its_arguments() {
        // Thread 7's timed wait entered before the decision; its timeout
        // pointer differs between the two (synthetic) traces. It still aligns,
        // so its different return is a victim, and thread 9's wake, which
        // lands after the timeout in the target and before it in the
        // baseline, is its partner.
        let wait = |timeout, post, result| {
            let mut sys = call(
                95,
                7,
                4,
                Sysno::futex,
                &[0x5000, 128, 0, timeout],
                Some(result),
                vec![futex(FutexOp::Wait)],
            );
            sys.post = Some(post);
            sys
        };
        let wake = |pre| {
            call(
                pre,
                9,
                6,
                Sysno::futex,
                &[0x5000, 129, 1],
                Some(1),
                vec![futex(FutexOp::Wake)],
            )
        };
        let target = trace(vec![wait(0x7000, 103, ETIMEDOUT), wake(105)]);
        let baseline = trace(vec![wake(101), wait(0x7100, 102, 0)]);
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| (a.thread, a.at))
                .collect::<Vec<_>>(),
            vec![(t(7), AccessPoint::Return), (t(9), AccessPoint::Entry)]
        );
    }

    #[test]
    fn a_thread_whose_calls_already_split_gives_no_lifetime_pair() {
        // Thread 5's call 10 differs between the runs, so its baseline call
        // 11 is not the call the kill kept it from making in the target.
        let app = |mut sys: SyscallRecord| {
            sys.pid = DetPid::from_raw(5);
            sys
        };
        let kill = |pre| {
            call(
                pre,
                3,
                20,
                Sysno::kill,
                &[5, 9],
                Some(0),
                vec![Access::Lifetime {
                    target: Task::Process(DetPid::from_raw(5)),
                    op: LifetimeOp::Signal { signo: 9 },
                }],
            )
        };
        let target = trace(vec![
            app(call(101, 5, 10, Sysno::getpid, &[], Some(5), Vec::new())),
            kill(103),
        ]);
        let baseline = trace(vec![
            app(call(101, 5, 10, Sysno::getppid, &[], Some(1), Vec::new())),
            app(call(103, 5, 11, Sysno::gettid, &[], Some(5), Vec::new())),
            kill(105),
        ]);
        for outcome in [None, Some(3), Some(5)] {
            let r = find(&target, &baseline, outcome);
            assert_ne!(r.status, RaceStatus::Found, "{outcome:?}: {}", r.reason);
        }
    }

    #[test]
    fn a_clock_read_victim_times_the_flip() {
        // Thread 5's clock read returns another time; nothing it touched
        // conflicts. The timing pair is it and thread 9's timed wait, whose
        // return order with it flips.
        let read = |digest, post| {
            let mut sys = call(
                101,
                5,
                10,
                Sysno::clock_gettime,
                &[1, 0x2000],
                Some(0),
                Vec::new(),
            );
            sys.returned = Some(ReturnedBytes {
                len: 16,
                digest,
                truncated: false,
            });
            sys.post = Some(post);
            sys
        };
        let sleep = |post| {
            let mut sys = call(
                99,
                9,
                4,
                Sysno::clock_nanosleep,
                &[1, 0, 0x1000, 0],
                Some(0),
                Vec::new(),
            );
            sys.post = Some(post);
            sys
        };
        let target = trace(vec![read(1, 102), sleep(104)]);
        let baseline = trace(vec![sleep(102), read(2, 104)]);
        let r = find(&target, &baseline, Some(5));
        assert_eq!(r.status, RaceStatus::Timing, "{}", r.reason);
        assert!(
            r.reason.starts_with("thread 5's clock_gettime returns a different time at event 102 in the two runs; the order that flips is two timed waits or clock reads: thread 5's clock_gettime and thread 9's clock_nanosleep"),
            "{}",
            r.reason
        );
        assert_eq!(r.order.unwrap().baseline, [1, 0]);
    }

    #[test]
    fn the_last_difference_before_a_control_flow_change_is_the_victim() {
        // Thread 7, the outcome thread, reads a pipe thread 8 writes, then
        // seeks fd 6 thread 9 closes; both results differ, and then thread
        // 7 makes a different call. The seek, the last difference before
        // the change, is the victim; the read pair is only listed.
        let mine = Some(file(7, 1));
        let write = |pre| {
            call(
                pre,
                8,
                5,
                Sysno::write,
                &[4, 0x1000, 4],
                Some(4),
                vec![pipe(file(7, 2), ChannelOp::Write)],
            )
        };
        let read = |pre, result| {
            call(
                pre,
                7,
                20,
                Sysno::read,
                &[3, 0x2000, 4],
                Some(result),
                vec![pipe(file(7, 1), ChannelOp::Read)],
            )
        };
        let lseek = |pre, result, open_file| {
            call(
                pre,
                7,
                21,
                Sysno::lseek,
                &[6, 0, 0],
                Some(result),
                vec![fd(6, open_file, FdOp::Use)],
            )
        };
        let next = |pre, fd_number| {
            call(
                pre,
                7,
                22,
                Sysno::write,
                &[fd_number, 0x3000, 4],
                Some(4),
                Vec::new(),
            )
        };
        let target = trace(vec![
            write(101),
            read(103, 4),
            close6(105, 9, 10, mine, 0),
            lseek(107, EBADF, None),
            next(109, 2),
        ]);
        let baseline = trace(vec![
            read(101, EAGAIN),
            lseek(103, 0, mine),
            next(105, 6),
            write(107),
            close6(109, 9, 10, mine, 0),
        ]);
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(
            r.accesses
                .iter()
                .map(|a| a.call.as_str())
                .collect::<Vec<_>>(),
            vec!["close(6)", "lseek(6, 0, SEEK_SET)"]
        );
        assert_eq!(
            r.also_reordered[0]
                .accesses
                .each_ref()
                .map(|a| a.call.as_str()),
            ["write(4, 0x1000, 4)", "read(3, 0x2000, 4)"]
        );
    }

    #[test]
    fn a_partner_whose_thread_changed_course_has_no_matching_call() {
        // Thread 9's call 9 differs between the runs, so its close (call
        // 10) has no counterpart in the baseline although the thread ran on.
        let (mut target, mut baseline) = close_then_read();
        let course = |sysno| TraceRecord::Syscall(call(100, 9, 9, sysno, &[], Some(9), Vec::new()));
        target.records.insert(1, course(Sysno::getpid));
        baseline.records.insert(1, course(Sysno::gettid));
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.accesses[0].baseline_differs_from, Some(9));
        assert_eq!(r.accesses[1].baseline_differs_from, None);
        let text = render_racing_accesses(&r);
        assert!(
            text.contains("In the baseline the order is reversed: thread 7's read at #101 returns 64, and thread 9's close has no matching call (thread 9's calls differ from its call 9 on)."),
            "{text}"
        );
    }

    #[test]
    fn channels_whose_data_gives_no_edge_are_named() {
        // A read that finds no write at all makes the channel's offsets
        // unknown.
        let lone = call(
            95,
            9,
            1,
            Sysno::read,
            &[3, 0x2000, 8],
            Some(8),
            vec![pipe(file(7, 1), ChannelOp::Read)],
        );
        let shared = ChannelKey::Shared(ChannelId::Pair(file(7, 1)));
        assert!(data_edges(&[&lone]).untracked.contains_key(&shared));

        // The pipe held bytes from before the trace: the first read returns
        // 8 bytes after a 4-byte write, so the offsets are unknown and
        // thread 8's later write gives thread 9's read no edge. The report
        // names the pipe instead of claiming no edge exists.
        let early = call(
            95,
            9,
            39,
            Sysno::read,
            &[3, 0x2000, 8],
            Some(8),
            vec![pipe(file(7, 1), ChannelOp::Read)],
        );
        let write = |pre| {
            call(
                pre,
                8,
                5,
                Sysno::write,
                &[4, 0x1000, 4],
                Some(4),
                vec![pipe(file(7, 2), ChannelOp::Write)],
            )
        };
        let read = |pre, result| {
            call(
                pre,
                9,
                40,
                Sysno::read,
                &[3, 0x2000, 4],
                Some(result),
                vec![pipe(file(7, 1), ChannelOp::Read)],
            )
        };
        let before = call(
            90,
            8,
            4,
            Sysno::write,
            &[4, 0x1000, 4],
            Some(4),
            vec![pipe(file(7, 2), ChannelOp::Write)],
        );
        let target = trace(vec![
            before.clone(),
            early.clone(),
            write(103),
            read(105, 4),
        ]);
        let baseline = trace(vec![before, early, read(101, EAGAIN), write(103)]);
        let r = find(&target, &baseline, Some(9));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.happens_before.as_ref().unwrap().target, unordered());
        assert_eq!(
            r.notes,
            vec!["Data through the pipe whose first end is file 1 opened by thread 7 gives no edge from event 96 on: a read returned data the trace does not show being written.".to_string()]
        );

        // A socket end whose peer is not recorded, used between the pair.
        let (mut target, mut baseline) = close_then_read();
        let send = |pre| {
            TraceRecord::Syscall(call(
                pre,
                8,
                3,
                Sysno::sendto,
                &[7, 0x1000, 4],
                Some(4),
                vec![Access::Channel {
                    open_file: file(8, 1),
                    id: None,
                    channel: ChannelKind::StreamSocket,
                    op: ChannelOp::Write,
                }],
            ))
        };
        target.records.push(send(102));
        baseline.records.push(send(102));
        let r = find(&target, &baseline, Some(7));
        assert_eq!(
            r.notes,
            vec!["Data through the stream socket end file 1 opened by thread 8 gives no edge: Hermit does not record the end it is connected to.".to_string()]
        );
        assert!(render_racing_accesses(&r).ends_with(&r.notes[0]));
    }

    #[test]
    fn an_fd_access_is_ordered_at_its_entry() {
        // Thread 7 dup3s another file onto fd 6, then writes the pipe that
        // thread 9's blocking read(6) returns. The read reached fd 6 at its
        // entry, before the write, so the pipe edge to its return does not
        // order the dup3 before the fd access.
        let (old, new) = (Some(file(7, 1)), Some(file(7, 9)));
        let dup2 = |pre| {
            call(
                pre,
                7,
                30,
                Sysno::dup3,
                &[5, 6],
                Some(6),
                vec![fd(6, old, FdOp::Close), fd(6, new, FdOp::Dup { source: 5 })],
            )
        };
        let write = |pre| {
            call(
                pre,
                7,
                31,
                Sysno::write,
                &[4, 0x1000, 4],
                Some(4),
                vec![pipe(file(7, 2), ChannelOp::Write)],
            )
        };
        let read = |pre, post, open_file| {
            let mut sys = call(
                pre,
                9,
                40,
                Sysno::read,
                &[6, 0x2000, 4],
                Some(4),
                vec![
                    fd(6, open_file, FdOp::Use),
                    pipe(file(7, 1), ChannelOp::Read),
                ],
            );
            sys.post = Some(post);
            sys
        };
        let target = trace(vec![dup2(101), read(103, 110, new), write(105)]);
        let baseline = trace(vec![write(99), read(101, 104, old), dup2(106)]);
        let r = find(&target, &baseline, Some(9));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.resource.as_ref().unwrap().kind, ResourceKind::Fd);
        assert_eq!(r.accesses[0].call, "dup3(5, 6, 0)");
        assert_eq!(r.happens_before.unwrap().target, unordered());
    }

    #[test]
    fn backward_edges_are_ignored_with_a_note() {
        let (target, baseline) = close_then_read();
        let mut target = target;
        target
            .records
            .push(edge(EdgeKind::FutexWake, (7, 103), (9, 101)));
        let r = find(&target, &baseline, Some(7));
        assert_eq!(r.status, RaceStatus::Found, "{}", r.reason);
        assert_eq!(r.happens_before.unwrap().target, unordered());
        assert_eq!(
            r.notes,
            vec!["The target trace has 1 edge(s) that do not run forward (the first from event 103 to event 101); they are ignored.".to_string()]
        );
    }

    /// The fdsan case of design section 8: thread 9 closes fd 6 a second
    /// time after thread 7 reused the number, so thread 7's lseek fails.
    fn fdsan() -> RacingAccesses {
        fdsan_with(Vec::new())
    }

    /// [`fdsan`] with more calls before the deciding event.
    fn fdsan_with(earlier: Vec<SyscallRecord>) -> RacingAccesses {
        let (pipe_end, misc) = (Some(file(9, 4)), Some(file(7, 3)));
        let mut prefix = earlier;
        prefix.extend([
            close6(7552, 9, 1400, pipe_end, 0),
            call(
                7571,
                7,
                380,
                Sysno::openat,
                &[-100i64 as u64, 0x2000, 2],
                Some(6),
                vec![fd(6, misc, opened("/tmp/misc"))],
            ),
        ]);
        let lseek = |pre, result, accesses| {
            call(
                pre,
                7,
                388,
                Sysno::lseek,
                &[6, 2048, 0],
                Some(result),
                accesses,
            )
        };
        let mut report = call(
            7593,
            7,
            389,
            Sysno::write,
            &[2, 0x4000, 41],
            Some(41),
            Vec::new(),
        );
        report.out_bytes = Some(b"fdsan: double-close of file descriptor 6\n".to_vec());
        let target = trace(
            [
                prefix.clone(),
                vec![
                    close6(7587, 9, 1412, misc, 0),
                    lseek(7589, EBADF, vec![fd(6, None, FdOp::Use)]),
                    report,
                ],
            ]
            .concat(),
        );
        let baseline = trace(
            [
                prefix,
                vec![
                    lseek(
                        7588,
                        2048,
                        vec![
                            fd(6, misc, FdOp::Use),
                            Access::OpenFile {
                                open_file: file(7, 3),
                                op: CursorOp::Seek,
                            },
                        ],
                    ),
                    call(
                        7590,
                        7,
                        389,
                        Sysno::write,
                        &[6, 0x5000, 512],
                        Some(512),
                        vec![fd(6, misc, FdOp::Use)],
                    ),
                    close6(7611, 9, 1412, None, EBADF),
                ],
            ]
            .concat(),
        );
        let window = Window {
            decision: 7586,
            outcome: Some(OutcomeRef {
                event: 7593,
                thread: t(7),
                text: "write(2, \"fdsan: double-close of file descriptor 6\")".to_string(),
            }),
        };
        find_racing_accesses(&target, &baseline, window, Some(t(7)))
    }

    #[test]
    fn renders_the_fdsan_case() {
        let text = render_racing_accesses(&fdsan());
        let expected = [
            "Racing accesses (fd 6 of process 3):",
            "  thread 9  #7587  close(6) = 0",
            "  thread 7  #7589  lseek(6, 2048, SEEK_SET) = -EBADF",
            "In the baseline the order is reversed: thread 7's lseek at #7588 returns 2048, and thread 9's close at #7611 returns -EBADF.",
            "Before them in the target: thread 9's close(6) = 0 released the number (#7552); thread 7's openat(AT_FDCWD, \"/tmp/misc\", 2) = 6 installed file 3 opened by thread 7 (/tmp/misc) (#7571).",
            "Thread 9 had already closed the number, so its close at #7587 closes the file thread 7 installed.",
            "Outcome: write(2, \"fdsan: double-close of file descriptor 6\") by thread 7 at #7593.",
            "Happens-before: unordered by the edges Hermit observes, in both orders: no edge Hermit records or derives connects thread 9's close to thread 7's lseek. Uncontended locks and atomics are not visible to Hermit.",
        ]
        .join("\n");
        assert_eq!(text, expected);
        assert!(!text.contains("data race"));
    }

    #[test]
    fn json_form_names_status_pair_order_and_verdicts() {
        let json = serde_json::to_value(fdsan()).unwrap();
        assert_eq!(json["status"], "found");
        assert_eq!(json["resource"]["kind"], "fd");
        assert_eq!(json["accesses"][1]["call"], "lseek(6, 2048, SEEK_SET)");
        assert_eq!(json["accesses"][1]["baseline_result"], "2048");
        assert_eq!(json["order"]["baseline"], serde_json::json!([1, 0]));
        assert_eq!(json["happens_before"]["target"]["ordered"], false);
        assert_eq!(json["outcome"]["event"], 7593);
        let back: RacingAccesses = serde_json::from_value(json).unwrap();
        assert_eq!(back, fdsan());
    }

    #[test]
    fn traces_that_cannot_answer_are_unavailable() {
        let (target, baseline) = close_then_read();
        let mut late = baseline.clone();
        late.header.from = S + 1;
        let r = find(&target, &late, Some(7));
        assert_eq!(r.status, RaceStatus::Unavailable);
        assert!(
            r.reason
                .contains("record both replays with --access-trace-from at or before 100"),
            "{}",
            r.reason
        );

        // A baseline cut short before the decision says so, rather than
        // that the traces differ.
        let mut cut = baseline.clone();
        cut.records.truncate(1);
        cut.end = None;
        let r = find(&target, &cut, Some(7));
        assert_eq!(r.status, RaceStatus::Unavailable);
        assert!(
            r.reason.starts_with("the baseline trace ends at event 91 without an end line, before the deciding event 100"),
            "{}",
            r.reason
        );

        // The prefix before the decision differs: the replays did not
        // follow one schedule.
        let mut other = baseline;
        if let TraceRecord::Syscall(open) = &mut other.records[0] {
            open.result = Some(7);
        }
        let r = find(&target, &other, Some(7));
        assert_eq!(r.status, RaceStatus::Unavailable);
        assert!(
            r.reason
                .starts_with("the two traces differ at event 90, before the deciding event 100"),
            "{}",
            r.reason
        );
    }
}
