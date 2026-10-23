/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Work and span of the schedule Detcore ran, opt-in with
//! `--work-span-report=<path>` (`Config::work_span_report`), computed from the
//! scheduler's own typed state, never from log text. The report is written
//! when the run ends normally (the scheduler finds nothing left to run); a run
//! ended by a refusal or a fatal error writes none.
//!
//! Turns commit one at a time (`Scheduler::unblock_guest`), so commit order is
//! a topological order of every edge below and no graph is stored. Each
//! committed turn of thread `T` ends a node whose weight is `T`'s own guest
//! CPU time since its previous commit (user plus system; clock jumps to
//! deadlines are excluded). Every request samples that clock; a child's first
//! request carries the clock it inherited, which becomes its baseline, so
//! inherited time is not counted again. The node's commit happens at
//!
//! ```text
//! commit(T) = max(previous commit(T) + weight, every other predecessor)
//! ```
//!
//! Work is the sum of the weights, and span the largest commit time.
//!
//! Edges in both spans, each certain:
//!
//! - program order: a thread's consecutive commits;
//! - spawn: the creator as far as it had run at the clone, to the child
//!   (`work_span_note_spawn`);
//! - exit to reap: a child process's completion (its ending `Exit` commit,
//!   joined with every earlier thread exit and retirement of the process),
//!   to the thread whose wait reaped exactly that child
//!   (`GlobalRequest::ConsumeChildWait`), from the reap on (`Segment`); a
//!   zombie adopted by a subreaper keeps its record;
//! - resource conflicts: the request's resource, for the `ResourceID` variants
//!   that name guest-visible state (`conflict_key`, an exhaustive match),
//!   including the access's own syscall, which runs after the commit.
//!
//! The upper span adds two conservative rules:
//!
//! - waiting: a thread whose request waited (it blocked, retried a call that
//!   found nothing to do, was parked outside the run queue, or its call ran
//!   outside the schedule) follows every commit, and every request, made by
//!   other threads while it waited,
//!   whatever it waited for (`wait_kind`, an exhaustive match; sleeps, exit
//!   permits, yields and other scheduler-only waits take none);
//! - signals: a turn that delivers a signal (`ResourceID::InboundSignal`,
//!   `ResourceID::WaitidSignals`) follows every commit so far and every live
//!   thread's latest request, whoever sent it.
//!
//! Nothing else is modelled, and every report says so (`Unmodelled`): what a
//! waiting thread waited for and who sent a signal (so `span_lo` can be
//! optimistic), and what passes without a wait (data, an end of file or a
//! peer's close found on a channel, a pending signal taken by `sigtimedwait`,
//! memory shared without a futex wait, files and devices), which neither span
//! orders.
//!
//! **Off, it costs nothing measurable.** The tool is unchanged; the global
//! handler checks one `Option` before sampling a request's clock, and the
//! scheduler one bool per commit, wait, spawn, reap and retirement.
//!
//! **Memory is bounded.** Per live thread, a handful of map entries, freed
//! when the scheduler retires it (`work_span_retire_thread`) and moved when a
//! non-leader execs (`work_span_transfer_thread`); a retired thread's work
//! since its last commit is counted then. Per live process, its completion so
//! far, freed with its last thread. Per live parent process, its latest child
//! exit and its zombies' exits, at most `MAX_EXITED_PER_PARENT`, moved to a
//! subreaper with an adopted zombie and freed once the parent's orphans are
//! placed (`work_span_parent_gone`); an orphan adopted by the container's init
//! has no guest reaper and leaves no record. A reap whose exit was dropped
//! follows the parent's latest child exit in the upper span. Per
//! resource, one `Access` (32 bytes) plus its key, at most `MAX_RESOURCES`;
//! past that the table is cleared and its latest time kept as a floor that a
//! resource seen again keeps as its write, in the upper span, until a real
//! write.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;

use super::Scheduler;
use crate::config::Config;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;

/// Most resources with a remembered access; past it the table is cleared
/// into `WorkSpanState::resource_floor`.
const MAX_RESOURCES: usize = 4096;

/// Most exited, unreaped children kept per parent; past it the oldest goes,
/// and its reap follows the parent's latest child exit in the upper span.
const MAX_EXITED_PER_PARENT: usize = 4096;

/// A time in both spans: `lo` with the certain edges only, `hi` with the
/// waiting rule too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Bounds {
    pub(crate) lo: u64,
    pub(crate) hi: u64,
}

impl Bounds {
    fn max(self, other: Bounds) -> Bounds {
        Bounds {
            lo: self.lo.max(other.lo),
            hi: self.hi.max(other.hi),
        }
    }

    fn plus(self, weight: u64) -> Bounds {
        Bounds {
            lo: self.lo + weight,
            hi: self.hi + weight,
        }
    }
}

/// The kind of predecessor that fixed a commit's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EdgeKind {
    /// The thread's own previous commit plus its work.
    Program,
    /// The creator's clone commit.
    Spawn,
    /// A child's exit, reaped by this thread.
    Reap,
    /// The last conflicting access to the request's resource.
    Resource,
    /// A signal delivery: whatever any thread had reached (upper span only).
    Signal,
    /// What other threads did while this one waited (upper span only).
    Waited,
}

/// What a thread that waited waited for (`wait_kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitKind {
    /// A polled descriptor (`ResourceID::InternalIOPolling`): a read, write,
    /// readiness wait or child wait the call retries.
    Poll,
    /// A futex wait.
    Futex,
    /// A call that ran outside the schedule (a background call, a vfork
    /// parent, an `rt_sigsuspend`).
    Outside,
    /// A signal to deliver, or a `waitid` waiting for one.
    Signal,
    /// Guest-visible state another thread holds (`conflict_key`).
    Resource,
    /// A vfork parent waiting for its child to exec or exit.
    VforkParent,
    /// A child wait the scheduler parks (`ResourceID::WaitChild`).
    ChildWait,
}

/// Dependencies the report does not model; every report lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Unmodelled {
    /// What a waiting thread waited for (a futex wake, a pipe write, a
    /// child's exit it did not reap): the upper span orders the
    /// thread after everything other threads did meanwhile; the lower span
    /// leaves it out, and so can be optimistic.
    WaitCauses,
    /// What a thread observes from a pipe, socket or other channel without
    /// waiting for it: data already there, an end of file, or a peer's close
    /// that a write finds (`EPIPE`, `ECONNRESET`).
    ChannelDataWithoutWaiting,
    /// Memory shared between threads or processes, written by one and read by
    /// another with no futex wait between.
    SharedMemoryWithoutWaiting,
    /// Who sent a signal: a delivery follows, in the upper span, the latest
    /// time any thread had reached, and in the lower span nothing. A signal
    /// already pending that a call takes without waiting (`sigtimedwait`,
    /// `sigwaitinfo`, `sigwait`) follows its sender in neither span.
    SignalSenders,
    /// The CPU time of a call that a thread was running outside the schedule
    /// when it was killed (its process's `exit_group`, or an exec): its
    /// clock arrives at host-timed moments, so work and both spans stop at
    /// the thread's last scheduled sample.
    KilledOutsideCalls,
    /// Data passed through regular files or devices that nothing else
    /// orders. Builds order a file's producer and consumer through a wait and
    /// a spawn, which both spans model.
    FilesAndDevices,
}

/// Every report's `Unmodelled` list.
const UNMODELLED: [Unmodelled; 6] = [
    Unmodelled::WaitCauses,
    Unmodelled::ChannelDataWithoutWaiting,
    Unmodelled::SharedMemoryWithoutWaiting,
    Unmodelled::SignalSenders,
    Unmodelled::KilledOutsideCalls,
    Unmodelled::FilesAndDevices,
];

/// Predecessors met inside a thread's current segment (a reap): from guest
/// CPU time `cpu` on, the thread's time is at least `cpu + offset`. Only the
/// largest offset per span is kept, which is all a later time needs.
#[derive(Clone, Copy, Debug, Default)]
struct Segment {
    lo: Option<i128>,
    hi: Option<i128>,
}

/// `offset` applied at guest CPU time `cpu`.
fn shifted(cpu: u64, offset: i128) -> u64 {
    u64::try_from((i128::from(cpu) + offset).max(0)).unwrap_or(u64::MAX)
}

/// Accesses to one resource.
#[derive(Clone, Copy, Debug, Default)]
struct Access {
    /// The last write's time.
    write: Bounds,
    /// The largest read time since that write.
    reads: Bounds,
}

/// Scheduler-side state of the work/span report.
#[derive(Debug, Default)]
pub struct WorkSpanState {
    pub(super) enabled: bool,
    /// Where to write the report (`Config::work_span_report`).
    report: Option<PathBuf>,
    /// Each thread's guest CPU time carried with its latest request.
    cpu_now: HashMap<DetTid, u64>,
    /// The latest guest CPU time of a thread whose call runs outside the
    /// schedule, kept from everyone until its continuation commits.
    cpu_outside: HashMap<DetTid, u64>,
    /// Each thread's guest CPU time at its latest commit (or first request).
    cpu_committed: HashMap<DetTid, u64>,
    /// Each thread's latest commit time.
    finish: HashMap<DetTid, Bounds>,
    /// A child thread's start: where its creator had reached at the clone
    /// (`Scheduler::work_span_note_spawn`).
    spawned: HashMap<DetTid, Bounds>,
    /// Per thread, the reaps met inside its current segment.
    segment: HashMap<DetTid, Segment>,
    /// Threads waiting on others, with the latest time other threads reached
    /// while they waited, and what they wait for.
    waiting: HashMap<DetTid, (u64, WaitKind)>,
    resources: HashMap<ResourceID, Access>,
    /// The latest access time of resources cleared from `resources`, which a
    /// resource seen again keeps as its write in the upper span.
    resource_floor: Bounds,
    resource_evictions: u64,
    /// Per thread, the resources its latest commit granted it. The access
    /// itself runs after that commit, so until the thread's next commit (or
    /// its retirement) a conflicting access follows the thread as far as it
    /// has run, and then that commit's time.
    pending_access: HashMap<DetTid, Vec<(ResourceID, Permission)>>,
    /// Per parent process, the exit times of its zombies.
    exited: HashMap<DetPid, BTreeMap<DetPid, Bounds>>,
    /// Per parent process, its latest child exit.
    latest_child_exit: HashMap<DetPid, Bounds>,
    /// Per live process, the latest final time of its threads so far (an
    /// `Exit` commit, or a retirement's last sample): a process is reaped only
    /// after all its threads are gone, so its exit record joins them.
    process_done: HashMap<DetPid, Bounds>,
    /// `MAX_EXITED_PER_PARENT`, smaller in tests.
    exited_cap: usize,
    work: u64,
    span: Bounds,
    commits: u64,
    /// Threads seen in all, retired ones included.
    threads: u64,
    /// How often each kind of edge fixed a commit's time, per span.
    binding_lo: BTreeMap<EdgeKind, u64>,
    binding_hi: BTreeMap<EdgeKind, u64>,
    /// Of the upper span's `Waited` bindings, how many each kind of wait made.
    binding_waited: BTreeMap<WaitKind, u64>,
}

impl WorkSpanState {
    /// Empty state; on when `--work-span-report` names a path.
    pub(super) fn new(cfg: &Config) -> Self {
        WorkSpanState {
            enabled: cfg.work_span_report.is_some(),
            report: cfg.work_span_report.clone(),
            exited_cap: MAX_EXITED_PER_PARENT,
            ..Default::default()
        }
    }
}

/// The report written at the end of the run.
#[derive(Debug, Serialize)]
pub(crate) struct WorkSpanReport {
    pub(crate) work_ns: u64,
    /// The span over the certain edges: a lower bound of the modelled
    /// dependencies, which can be optimistic for the unmodelled ones.
    pub(crate) span_lo_ns: u64,
    /// The span with the waiting rule: conservative for the modelled
    /// dependencies and for what waits wait on; it does not order data passed
    /// without a wait (`unmodelled`).
    pub(crate) span_hi_ns: u64,
    /// work / span_hi.
    pub(crate) parallelism_lo: f64,
    /// work / span_lo.
    pub(crate) parallelism_hi: f64,
    pub(crate) commits: u64,
    pub(crate) threads: u64,
    pub(crate) binding_lo: BTreeMap<EdgeKind, u64>,
    pub(crate) binding_hi: BTreeMap<EdgeKind, u64>,
    pub(crate) binding_waited: BTreeMap<WaitKind, u64>,
    /// Times the resource table was cleared past `MAX_RESOURCES`.
    pub(crate) resource_evictions: u64,
    /// Dependencies the report does not model.
    pub(crate) unmodelled: Vec<Unmodelled>,
}

/// The key a resource conflicts under, or `None` for a scheduler-only
/// resource. Exhaustive, so that a new variant has to be classified.
fn conflict_key(resource: &ResourceID) -> Option<&ResourceID> {
    match resource {
        ResourceID::FileContents(_)
        | ResourceID::FileMetadata(_)
        | ResourceID::DirectoryContents(_)
        | ResourceID::MemAddrSpace(_)
        | ResourceID::Path(_)
        | ResourceID::PathsTransitive(_)
        | ResourceID::Device(_) => Some(resource),
        ResourceID::Exit { .. }
        | ResourceID::ParentContinue { .. }
        | ResourceID::SleepUntil(_)
        | ResourceID::InternalIOPolling
        | ResourceID::TraceReplay
        | ResourceID::FutexWait
        | ResourceID::BlockingExternalIO(_)
        | ResourceID::BlockingVfork(_)
        | ResourceID::VforkFailed(_)
        | ResourceID::BlockedExternalContinue(_)
        | ResourceID::PriorityChangePoint(..)
        | ResourceID::WaitChild { .. }
        | ResourceID::WaitPhysicalChild(_)
        | ResourceID::InboundSignal(_)
        | ResourceID::WaitidSignals(_)
        | ResourceID::SchedYield
        | ResourceID::HappensBeforeCheckpoint(_)
        | ResourceID::HappensBeforeSyscallCheckpoint { .. }
        | ResourceID::HappensBeforePosthookInterrupted { .. }
        | ResourceID::BlockingRtSigsuspend(_)
        | ResourceID::InGuestFatalAlarm => None,
    }
}

/// What a request that waits (blocked, polled or parked) waits for, or `None`
/// when it takes no waiting edge: a sleep, an exit permit, a yield and other
/// scheduler-only waits. The most conservative kind among the request's
/// resources, so the answer does not depend on map order.
fn wait_kind(request: &Resources) -> Option<WaitKind> {
    request
        .resources
        .keys()
        .filter_map(resource_wait_kind)
        .max()
}

/// `wait_kind` for one resource. Exhaustive, so that a new variant has to be
/// classified.
fn resource_wait_kind(resource: &ResourceID) -> Option<WaitKind> {
    match resource {
        ResourceID::FileContents(_)
        | ResourceID::FileMetadata(_)
        | ResourceID::DirectoryContents(_)
        | ResourceID::MemAddrSpace(_)
        | ResourceID::Path(_)
        | ResourceID::PathsTransitive(_)
        | ResourceID::Device(_) => Some(WaitKind::Resource),
        ResourceID::InternalIOPolling => Some(WaitKind::Poll),
        ResourceID::FutexWait => Some(WaitKind::Futex),
        // The kernel-run blocking calls (`ResourceID::is_kernel_run_blocking`),
        // and the continuation a backgrounded call files once its kernel call
        // has returned, which still waits outside the schedule for its grant.
        ResourceID::BlockingExternalIO(_)
        | ResourceID::BlockingVfork(_)
        | ResourceID::BlockedExternalContinue(_)
        | ResourceID::BlockingRtSigsuspend(_) => Some(WaitKind::Outside),
        ResourceID::InboundSignal(_) | ResourceID::WaitidSignals(_) => Some(WaitKind::Signal),
        ResourceID::ParentContinue { .. } => Some(WaitKind::VforkParent),
        ResourceID::WaitChild { .. } | ResourceID::WaitPhysicalChild(_) => {
            Some(WaitKind::ChildWait)
        }
        ResourceID::Exit { .. }
        | ResourceID::SleepUntil(_)
        | ResourceID::TraceReplay
        | ResourceID::VforkFailed(_)
        | ResourceID::PriorityChangePoint(..)
        | ResourceID::SchedYield
        | ResourceID::HappensBeforeCheckpoint(_)
        | ResourceID::HappensBeforeSyscallCheckpoint { .. }
        | ResourceID::HappensBeforePosthookInterrupted { .. }
        | ResourceID::InGuestFatalAlarm => None,
    }
}

/// Note `kind` as the binding predecessor if `candidate` exceeds `best`.
fn bind(best: &mut (u64, EdgeKind), candidate: u64, kind: EdgeKind) {
    if candidate > best.0 {
        *best = (candidate, kind);
    }
}

/// What an arriving request is, for `Scheduler::work_span_note_arrival`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Arrival<'a> {
    /// The thread's own `StartNewThread`.
    StartsThread,
    /// A retry of a call that found nothing to do (`poll_attempt > 0`).
    Retry(&'a Resources),
    /// `ReconnectExec` or `RetireExec`, whose clock still belongs to the
    /// thread that called exec (`Scheduler::work_span_transfer_thread` moves
    /// its state).
    ExecTransfer,
    /// Any other request.
    Other,
}

impl Scheduler {
    /// A request from `tid` arrives, carrying guest CPU time `cpu`. Every
    /// request samples the thread's clock. The first sets the baseline, and a
    /// `StartNewThread` sets it even before the scheduler registers the thread
    /// (a child can start before its creator's registration returns), so the
    /// baseline is the clock the child inherited whatever the host's timing,
    /// and inherited time is never counted as its work. A thread woken by what
    /// this one is doing may be granted before this one commits again, so the
    /// waiting threads follow this one as far as it has run. A retry begins a
    /// wait: from here on the thread follows what other threads do, although
    /// it stays in the run queue until the scheduler skips it.
    ///
    /// Only a thread inside its own segment, between a commit and its next
    /// request, sends requests at scheduled moments. Every other request
    /// arrives at a host-timed moment, so it raises no one and changes
    /// nothing another thread reads, and the report stays a function of
    /// commit order:
    /// - a child's `StartNewThread`, which can arrive after other threads
    ///   have taken turns, sets the baseline and nothing else (its creator's
    ///   own request raised the waiters as far as the clone);
    /// - a thread whose latest commit sent its call outside the schedule
    ///   keeps its latest sample to itself until its continuation commits, at
    ///   a scheduled point (the continuation's request is its last before
    ///   that commit);
    /// - a thread whose `Exit` has committed (`committed_exits`), whose
    ///   deregistration and robust-list wakes follow at host-timed moments,
    ///   keeps nothing: it does no more work, and no one waits on it;
    /// - an exec transfer, whose clock is the former thread's, keeps nothing;
    /// - a thread the scheduler does not know keeps nothing.
    pub(crate) fn work_span_note_arrival(&mut self, tid: DetTid, cpu: u64, arrival: Arrival<'_>) {
        if !self.work_span.enabled || matches!(arrival, Arrival::ExecTransfer) {
            return;
        }
        // A request from a thread the scheduler does not know (one retired,
        // or a child not registered yet) keeps nothing, except a child's
        // baseline.
        let known = self.next_turns.contains_key(&tid);
        let state = &mut self.work_span;
        if (known || matches!(arrival, Arrival::StartsThread))
            && !state.cpu_committed.contains_key(&tid)
        {
            state.threads += 1;
            state.cpu_committed.insert(tid, cpu);
        }
        if !known || matches!(arrival, Arrival::StartsThread) || self.committed_exits.contains(&tid)
        {
            return;
        }
        let state = &mut self.work_span;
        if matches!(state.waiting.get(&tid), Some((_, WaitKind::Outside))) {
            state.cpu_outside.insert(tid, cpu);
            return;
        }
        self.work_span.cpu_now.insert(tid, cpu);
        let ready = Self::work_span_ready_hi(&self.work_span, tid);
        self.work_span_raise_waiters(tid, ready);
        if let Arrival::Retry(request) = arrival
            && let Some(kind) = wait_kind(request)
        {
            self.work_span.waiting.entry(tid).or_insert((0, kind));
        }
    }

    /// Every other thread whose request waits (blocked, polling, or parked
    /// outside the run queue: a futex waiter, a parked child wait) follows
    /// `by`, which has reached `at` in the upper span.
    fn work_span_raise_waiters(&mut self, by: DetTid, at: u64) {
        let blocked: Vec<(DetTid, WaitKind)> = self
            .next_turns
            .iter()
            .filter(|(other, _)| **other != by && !self.run_queue.contains_tid(**other))
            .filter_map(|(other, turn)| {
                let kind = turn
                    .req
                    .try_read_with(|request| request.as_ref().ok().and_then(wait_kind))
                    .flatten()?;
                Some((*other, kind))
            })
            .collect();
        let state = &mut self.work_span;
        for (other, kind) in blocked {
            state.waiting.entry(other).or_insert((0, kind));
        }
        for (other, (waited, _)) in state.waiting.iter_mut() {
            if *other != by {
                *waited = (*waited).max(at);
            }
        }
    }

    /// `tid`'s time at its guest CPU time `cpu`, inside its current segment:
    /// its latest commit (or its start) plus its work since, or later where a
    /// reap met earlier in the segment says so.
    fn work_span_time_at(state: &WorkSpanState, tid: DetTid, cpu: u64) -> Bounds {
        let base = state
            .finish
            .get(&tid)
            .or_else(|| state.spawned.get(&tid))
            .copied()
            .unwrap_or_default();
        let committed = state.cpu_committed.get(&tid).copied().unwrap_or(cpu);
        let mut at = base.plus(cpu.saturating_sub(committed));
        if let Some(segment) = state.segment.get(&tid) {
            if let Some(offset) = segment.lo {
                at.lo = at.lo.max(shifted(cpu, offset));
            }
            if let Some(offset) = segment.hi {
                at.hi = at.hi.max(shifted(cpu, offset));
            }
        }
        at
    }

    /// `tid`'s time at its latest sample: where its current segment has
    /// reached.
    fn work_span_reached(state: &WorkSpanState, tid: DetTid) -> Bounds {
        let cpu = state
            .cpu_now
            .get(&tid)
            .or_else(|| state.cpu_committed.get(&tid))
            .copied()
            .unwrap_or(0);
        Self::work_span_time_at(state, tid, cpu)
    }

    /// `work_span_reached`, in the upper span.
    fn work_span_ready_hi(state: &WorkSpanState, tid: DetTid) -> u64 {
        Self::work_span_reached(state, tid).hi
    }

    /// From its guest CPU time `cpu` on, `tid`'s segment follows `pred`: in
    /// both spans, or (`lo` `None`) in the upper span only.
    fn work_span_anchor(
        state: &mut WorkSpanState,
        tid: DetTid,
        cpu: u64,
        lo: Option<u64>,
        hi: u64,
    ) {
        let segment = state.segment.entry(tid).or_default();
        let raise = |slot: &mut Option<i128>, at: u64| {
            let offset = i128::from(at) - i128::from(cpu);
            if slot.is_none_or(|current| offset > current) {
                *slot = Some(offset);
            }
        };
        if let Some(lo) = lo {
            raise(&mut segment.lo, lo);
        }
        raise(&mut segment.hi, hi);
    }

    /// `child` was created by `creator`: the child starts where the creator
    /// had reached at the clone. A clone without a turn of its own is
    /// registered by the creator's own request, which has just sampled its
    /// clock. A vfork's registration comes from the child, and its parent is
    /// outside the schedule from its `BlockingVfork` commit, immediately
    /// before the clone, so the parent's latest sample is that commit's.
    pub(crate) fn work_span_note_spawn(&mut self, creator: DetTid, child: DetTid) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        let start = Self::work_span_reached(state, creator);
        state.spawned.insert(child, start);
        state.finish.remove(&child);
        state.waiting.remove(&child);
        state.segment.remove(&child);
    }

    /// `reaper`'s wait reaped `child` of `parent` (`GlobalRequest::ConsumeChildWait`),
    /// with the reaper's guest CPU time `cpu` at the reap: from there on, it
    /// follows the child's exit. Without the child's exit record (dropped past
    /// `MAX_EXITED_PER_PARENT`, or pruned), the upper span follows the
    /// parent's latest child exit, which is no earlier.
    pub(crate) fn work_span_note_reap(
        &mut self,
        reaper: DetTid,
        cpu: u64,
        parent: DetPid,
        child: DetPid,
    ) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        match state
            .exited
            .get_mut(&parent)
            .and_then(|children| children.remove(&child))
        {
            Some(exit) => Self::work_span_anchor(state, reaper, cpu, Some(exit.lo), exit.hi),
            None => {
                if let Some(latest) = state.latest_child_exit.get(&parent).copied() {
                    Self::work_span_anchor(state, reaper, cpu, None, latest.hi);
                }
            }
        }
    }

    /// `child` of `parent` was reaped, by a wait or automatically
    /// (`Scheduler::consume_child_wait`): its exit is forgotten.
    pub(super) fn work_span_forget_exit(&mut self, parent: DetPid, child: DetPid) {
        if !self.work_span.enabled {
            return;
        }
        if let Some(children) = self.work_span.exited.get_mut(&parent) {
            children.remove(&child);
        }
    }

    /// `tid`'s request `request` waits (it blocked or polls).
    pub(super) fn work_span_note_wait(&mut self, tid: DetTid, request: &Resources) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        if let Some(kind) = wait_kind(request) {
            state.waiting.entry(tid).or_insert((0, kind));
        }
    }

    /// `tid`'s turn commits with request `request`.
    pub(super) fn work_span_commit(&mut self, tid: DetTid, request: Option<&Resources>) {
        if !self.work_span.enabled {
            return;
        }
        // The process this commit's exit ends, that process's parent, and the
        // parent's children whose exits still wait to be reaped.
        let ends = request
            .and_then(|request| {
                request
                    .resources
                    .keys()
                    .find_map(|resource| match resource {
                        ResourceID::Exit { group, .. } => {
                            self.work_span_ending_process(tid, *group)
                        }
                        _ => None,
                    })
            })
            .and_then(|process| {
                // An orphan adopted by the container's init has no guest
                // reaper, and its recorded parent is stale.
                if self.thread_tree.is_init_orphan(process) {
                    return None;
                }
                let parent = self.thread_tree.parent_process(&process)?;
                let waitable: Vec<DetPid> = self
                    .thread_tree
                    .unreaped_children_of(parent)
                    .into_iter()
                    .filter(|child| {
                        self.logically_exited_processes.contains(child)
                            && !self.autoreaped_exits.contains(child)
                    })
                    .collect();
                Some((process, parent, waitable))
            });
        // The process whose thread this `Exit` ends, ending the process or
        // not.
        let own_exit = request
            .is_some_and(|request| {
                request
                    .resources
                    .keys()
                    .any(|resource| matches!(resource, ResourceID::Exit { .. }))
            })
            .then(|| self.registered_process(tid))
            .flatten();
        let state = &mut self.work_span;
        if let Some(cpu) = state.cpu_outside.remove(&tid) {
            state.cpu_now.insert(tid, cpu);
        }
        // No sample since the baseline (a child's first turn): no work yet.
        // No baseline at all: none is invented, the next request sets it.
        let baseline = state.cpu_committed.get(&tid).copied();
        let cpu = state.cpu_now.get(&tid).copied().or(baseline).unwrap_or(0);
        let weight = baseline.map_or(0, |baseline| cpu.saturating_sub(baseline));
        let segment = state.segment.remove(&tid).unwrap_or_default();
        if baseline.is_some() {
            state.cpu_committed.insert(tid, cpu);
        }

        let (previous, first_kind) = match state.finish.get(&tid) {
            Some(previous) => (*previous, EdgeKind::Program),
            None => (
                state.spawned.remove(&tid).unwrap_or_default(),
                EdgeKind::Spawn,
            ),
        };
        let ready = previous.plus(weight);
        // The accesses this thread's previous commit granted ran in the
        // segment that ends here.
        Self::work_span_settle_accesses(state, tid, ready.hi);
        let mut lo = (ready.lo, first_kind);
        let mut hi = (ready.hi, first_kind);
        if let Some(offset) = segment.lo {
            bind(&mut lo, shifted(cpu, offset), EdgeKind::Reap);
        }
        if let Some(offset) = segment.hi {
            bind(&mut hi, shifted(cpu, offset), EdgeKind::Reap);
        }
        // Which kind of wait a `Waited` binding of this commit came from.
        let mut waited_kind = None;
        if let Some((time, kind)) = state.waiting.remove(&tid)
            && time > hi.0
        {
            hi = (time, EdgeKind::Waited);
            waited_kind = Some(kind);
        }
        // A signal delivery follows its sender, whoever that was: every commit
        // so far, and every live thread as far as it has run.
        if request.is_some_and(|request| {
            request.resources.keys().any(|resource| {
                matches!(
                    resource,
                    ResourceID::InboundSignal(_) | ResourceID::WaitidSignals(_)
                )
            })
        }) {
            let reached = state
                .cpu_now
                .keys()
                .filter(|other| **other != tid)
                .map(|other| Self::work_span_ready_hi(state, *other))
                .fold(state.span.hi, u64::max);
            bind(&mut hi, reached, EdgeKind::Signal);
        }
        let conflicts: Vec<(&ResourceID, &Permission)> = request
            .map(|request| {
                request
                    .resources
                    .iter()
                    .filter_map(|(resource, permission)| {
                        conflict_key(resource).map(|key| (key, permission))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (key, permission) in &conflicts {
            match state.resources.get(*key) {
                Some(access) => {
                    let before = match permission {
                        Permission::R => access.write,
                        Permission::W | Permission::RW => access.write.max(access.reads),
                    };
                    bind(&mut lo, before.lo, EdgeKind::Resource);
                    bind(&mut hi, before.hi, EdgeKind::Resource);
                }
                // Perhaps cleared past `MAX_RESOURCES`: the upper span takes
                // the cleared table's latest time.
                None => bind(&mut hi, state.resource_floor.hi, EdgeKind::Resource),
            }
            // A conflicting access still running in another thread's segment
            // ends no earlier than where that thread has reached.
            let running: Vec<DetTid> = state
                .pending_access
                .iter()
                .filter(|(other, accesses)| {
                    **other != tid
                        && accesses.iter().any(|(pending, held)| {
                            pending == *key
                                && (matches!(held, Permission::W | Permission::RW)
                                    || matches!(permission, Permission::W | Permission::RW))
                        })
                })
                .map(|(other, _)| *other)
                .collect();
            for other in running {
                let reached = Self::work_span_ready_hi(state, other);
                bind(&mut hi, reached, EdgeKind::Resource);
            }
        }

        let commit = Bounds { lo: lo.0, hi: hi.0 };
        *state.binding_lo.entry(lo.1).or_default() += 1;
        *state.binding_hi.entry(hi.1).or_default() += 1;
        if hi.1 == EdgeKind::Waited
            && let Some(kind) = waited_kind
        {
            *state.binding_waited.entry(kind).or_default() += 1;
        }
        let granted: Vec<(ResourceID, Permission)> = conflicts
            .iter()
            .map(|(key, permission)| ((*key).clone(), (*permission).clone()))
            .collect();
        for (key, permission) in conflicts {
            if !state.resources.contains_key(key) && state.resources.len() >= MAX_RESOURCES {
                for access in state.resources.values() {
                    state.resource_floor = state.resource_floor.max(access.write).max(access.reads);
                }
                state.resources = HashMap::new();
                state.resource_evictions += 1;
            }
            // A resource seen again after a clear may have a forgotten
            // writer: until a write supersedes it, the upper span keeps the
            // floor as its write.
            let floor = state.resource_floor.hi;
            let access = state.resources.entry(key.clone()).or_insert(Access {
                write: Bounds { lo: 0, hi: floor },
                reads: Bounds::default(),
            });
            match permission {
                Permission::R => access.reads = access.reads.max(commit),
                Permission::W | Permission::RW => {
                    access.write = commit;
                    access.reads = Bounds::default();
                }
            }
        }
        if granted.is_empty() {
            state.pending_access.remove(&tid);
        } else {
            state.pending_access.insert(tid, granted);
        }
        state.finish.insert(tid, commit);
        state.work += weight;
        state.span = state.span.max(commit);
        state.commits += 1;
        self.work_span_raise_waiters(tid, commit.hi);
        let state = &mut self.work_span;
        // A granted kernel-run blocking call sends the call outside the
        // schedule.
        if request.is_some_and(|request| {
            request
                .resources
                .keys()
                .any(ResourceID::is_kernel_run_blocking)
        }) {
            state.waiting.insert(tid, (0, WaitKind::Outside));
        }
        if let Some(process) = own_exit {
            let done = state.process_done.entry(process).or_default();
            *done = done.max(commit);
        }
        if let Some((process, parent, waitable)) = ends {
            // The process ends when its last thread does: after every thread
            // that exited or was retired before.
            let commit = state
                .process_done
                .get(&process)
                .map_or(commit, |done| done.max(commit));
            let exited = state.exited.entry(parent).or_default();
            // Children reaped some other way (automatically, or by a
            // subreaper) go.
            exited.retain(|child, _| waitable.contains(child));
            exited.insert(process, commit);
            while exited.len() > state.exited_cap {
                exited.pop_first();
            }
            let latest = state.latest_child_exit.entry(parent).or_default();
            *latest = latest.max(commit);
        }
    }

    /// `tid`'s granted accesses ran in a segment that reached `at` in the
    /// upper span: their resources keep that as the access's time.
    fn work_span_settle_accesses(state: &mut WorkSpanState, tid: DetTid, at: u64) {
        for (key, permission) in state.pending_access.remove(&tid).unwrap_or_default() {
            if let Some(access) = state.resources.get_mut(&key) {
                match permission {
                    Permission::R => access.reads.hi = access.reads.hi.max(at),
                    Permission::W | Permission::RW => access.write.hi = access.write.hi.max(at),
                }
            }
        }
    }

    /// The scheduler retires `tid` (`Scheduler::logically_kill_thread`): it
    /// commits no more. Its work since its last commit, sampled by its last
    /// scheduled request (a thread killed while parked, by an `exit_group` or
    /// an exec), is counted once here (`work_span_finish_tail`), its process's
    /// completion and exit record join it, and its state goes. The process's
    /// own state (its completion, and its records as a parent) goes when its
    /// last thread has left and its orphans are placed
    /// (`work_span_parent_gone`).
    pub(super) fn work_span_retire_thread(&mut self, tid: DetTid, process: DetPid) {
        if !self.work_span.enabled {
            return;
        }
        let process = self.registered_process(tid).unwrap_or(process);
        let parent = self.thread_tree.parent_process(&process);
        let state = &mut self.work_span;
        let tail = Self::work_span_finish_tail(state, tid);
        Self::work_span_settle_accesses(state, tid, tail.hi);
        let done = state.process_done.entry(process).or_default();
        *done = done.max(tail);
        // A sibling killed after its process's ending exit was recorded (an
        // `exit_group`): the record, and the parent's latest child exit, join
        // it. An existing record only: a dead parent's state is not recreated.
        if let Some(parent) = parent
            && let Some(record) = state
                .exited
                .get_mut(&parent)
                .and_then(|children| children.get_mut(&process))
        {
            *record = record.max(tail);
            if let Some(latest) = state.latest_child_exit.get_mut(&parent) {
                *latest = latest.max(tail);
            }
        }
        state.segment.remove(&tid);
        state.cpu_now.remove(&tid);
        state.cpu_outside.remove(&tid);
        state.cpu_committed.remove(&tid);
        state.finish.remove(&tid);
        state.spawned.remove(&tid);
        state.waiting.remove(&tid);
        state.pending_access.remove(&tid);
    }

    /// `tid`'s time at its last scheduled sample, its work since its last
    /// commit counted (once: its baseline moves to the sample), and the span
    /// raised to it.
    fn work_span_finish_tail(state: &mut WorkSpanState, tid: DetTid) -> Bounds {
        let reached = Self::work_span_reached(state, tid);
        if let (Some(cpu), Some(base)) = (state.cpu_now.get(&tid), state.cpu_committed.get(&tid)) {
            state.work += cpu.saturating_sub(*base);
            state.cpu_committed.insert(tid, *cpu);
        }
        state.span = state.span.max(reached);
        reached
    }

    /// Process `parent` is gone and its orphans are placed
    /// (`Scheduler::reparent_orphans`): its completion and its state as a
    /// parent go. Both of the scheduler's process-exit paths call this, the
    /// thread tree's own process for a raw clone child included, so the
    /// completion is freed under the key it was recorded with.
    pub(super) fn work_span_parent_gone(&mut self, parent: DetPid) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        state.process_done.remove(&parent);
        state.exited.remove(&parent);
        state.latest_child_exit.remove(&parent);
    }

    /// `child` of the exited process `former` was adopted by `reaper`: its
    /// exit record, if it has exited, moves with it. A record dropped past
    /// the cap leaves the former parent's latest child exit, which the
    /// adopter's then covers, in the upper span.
    pub(super) fn work_span_adopt(&mut self, former: DetPid, reaper: DetPid, child: DetPid) {
        let exited_child = self.logically_exited_processes.contains(&child);
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        match state
            .exited
            .get_mut(&former)
            .and_then(|children| children.remove(&child))
        {
            Some(record) => {
                let adopted = state.exited.entry(reaper).or_default();
                adopted.insert(child, record);
                while adopted.len() > state.exited_cap {
                    adopted.pop_first();
                }
                let latest = state.latest_child_exit.entry(reaper).or_default();
                *latest = latest.max(record);
            }
            None if exited_child => {
                if let Some(former_latest) = state.latest_child_exit.get(&former).copied() {
                    let latest = state.latest_child_exit.entry(reaper).or_default();
                    *latest = latest.max(former_latest);
                }
            }
            None => {}
        }
    }

    /// A non-leader `caller` exec'd and survives as the process's `leader`
    /// (`Scheduler::reconnect_exec`): its timeline goes on under the leader's
    /// tid.
    pub(super) fn work_span_transfer_thread(&mut self, caller: DetTid, leader: DetTid) {
        let state = &mut self.work_span;
        if !state.enabled {
            return;
        }
        // The former leader, which exec replaces, does no more work.
        if leader != caller {
            let tail = Self::work_span_finish_tail(state, leader);
            Self::work_span_settle_accesses(state, leader, tail.hi);
        }
        fn rekey<V>(map: &mut HashMap<DetTid, V>, caller: DetTid, leader: DetTid) {
            map.remove(&leader);
            if let Some(value) = map.remove(&caller) {
                map.insert(leader, value);
            }
        }
        rekey(&mut state.cpu_now, caller, leader);
        rekey(&mut state.cpu_outside, caller, leader);
        rekey(&mut state.cpu_committed, caller, leader);
        rekey(&mut state.finish, caller, leader);
        rekey(&mut state.spawned, caller, leader);
        rekey(&mut state.segment, caller, leader);
        rekey(&mut state.waiting, caller, leader);
        rekey(&mut state.pending_access, caller, leader);
    }

    /// The process `tid`'s exit ends, if it ends one: an `exit_group`, or the
    /// exit of its process's last live thread.
    fn work_span_ending_process(&self, tid: DetTid, group: bool) -> Option<DetPid> {
        let process = self.registered_process(tid)?;
        let last = group
            || !self
                .thread_tree
                .my_thread_group(&process)
                .into_iter()
                .any(|other| other != tid && self.next_turns.contains_key(&other));
        last.then_some(process)
    }

    /// The report, from the commits so far.
    pub(crate) fn work_span_report(&self) -> Option<WorkSpanReport> {
        let state = &self.work_span;
        if !state.enabled {
            return None;
        }
        let ratio = |span: u64| {
            if span == 0 {
                0.0
            } else {
                state.work as f64 / span as f64
            }
        };
        Some(WorkSpanReport {
            work_ns: state.work,
            span_lo_ns: state.span.lo,
            span_hi_ns: state.span.hi,
            parallelism_lo: ratio(state.span.hi),
            parallelism_hi: ratio(state.span.lo),
            commits: state.commits,
            threads: state.threads,
            binding_lo: state.binding_lo.clone(),
            binding_hi: state.binding_hi.clone(),
            binding_waited: state.binding_waited.clone(),
            resource_evictions: state.resource_evictions,
            unmodelled: UNMODELLED.to_vec(),
        })
    }

    /// Write the report to `--work-span-report`'s path, if set.
    pub(crate) fn write_work_span_report(&self) {
        let (Some(report), Some(path)) = (self.work_span_report(), self.work_span.report.as_ref())
        else {
            return;
        };
        let json = serde_json::to_string_pretty(&report).expect("the report serializes");
        if let Err(err) = std::fs::write(path, json + "\n") {
            tracing::warn!(
                "[work-span] cannot write the report to {}: {err}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::resources::Device;
    use crate::types::FutexID;
    use crate::types::LogicalTime;
    use crate::types::MmId;
    use crate::types::SigWrapper;

    fn tid(raw: i32) -> DetTid {
        DetTid::from_raw(raw)
    }

    fn pid(raw: i32) -> DetPid {
        DetPid::from_raw(raw)
    }

    /// Process 1 with children 2 and 3, the report on.
    fn family() -> Scheduler {
        let mut sched = Scheduler::new(&Config {
            work_span_report: Some(PathBuf::from("/nonexistent/report.json")),
            ..Config::default()
        });
        assert!(sched.work_span.enabled);
        for child in [1, 2, 3] {
            sched.thread_tree.add_child_with_wait_metadata(
                tid(1),
                tid(child),
                true,
                false,
                libc::SIGCHLD,
            );
            register(&mut sched, child);
        }
        sched
    }

    fn register(sched: &mut Scheduler, thread: i32) {
        sched.next_turns.insert(
            tid(thread),
            super::super::ThreadNextTurn {
                dettid: tid(thread),
                child_tid_addr: 0,
                req: crate::ivar::Ivar::new(),
                resp: crate::ivar::Ivar::new(),
                protocol: Default::default(),
            },
        );
    }

    fn request(thread: i32, resource: Option<(ResourceID, Permission)>) -> Resources {
        let mut request = Resources::new(tid(thread));
        if let Some((resource, permission)) = resource {
            request.insert(resource, permission);
        }
        request
    }

    /// `thread` has used `cpu` in all, and its turn commits with `resource`.
    /// An exit leaves a zombie, as the scheduler's logical exit does.
    fn commit(
        sched: &mut Scheduler,
        thread: i32,
        cpu: u64,
        resource: Option<(ResourceID, Permission)>,
    ) {
        let exits = matches!(resource, Some((ResourceID::Exit { .. }, _)));
        sched.work_span_note_arrival(tid(thread), cpu, Arrival::Other);
        sched.work_span_commit(tid(thread), Some(&request(thread, resource)));
        if exits {
            sched.logically_exited_processes.insert(pid(thread));
        }
    }

    fn exit(thread: i32) -> Option<(ResourceID, Permission)> {
        Some((
            ResourceID::Exit {
                group: true,
                process: pid(thread),
                mm: MmId::initial(tid(thread)),
            },
            Permission::W,
        ))
    }

    fn poll(thread: i32) -> Resources {
        request(thread, Some((ResourceID::InternalIOPolling, Permission::W)))
    }

    fn report(sched: &Scheduler) -> (u64, u64, u64) {
        let report = sched.work_span_report().unwrap();
        (report.work_ns, report.span_lo_ns, report.span_hi_ns)
    }

    /// `thread`'s latest commit time, (lower span, upper span).
    fn finish(sched: &Scheduler, thread: i32) -> (u64, u64) {
        let bounds = sched.work_span.finish[&tid(thread)];
        (bounds.lo, bounds.hi)
    }

    /// Two children forked at the same point run in parallel: the span is the
    /// parent's work, then the longer child, then the parent's work after it
    /// reaps both; the work is the sum.
    #[test]
    fn forked_children_run_in_parallel_and_their_reaps_join() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 1, 100, None); // the clone of 2 and 3
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for child in [2, 3] {
            commit(&mut sched, child, 0, None);
        }
        commit(&mut sched, 2, 1_000, None);
        commit(&mut sched, 2, 1_000, exit(2));
        commit(&mut sched, 3, 300, None);
        commit(&mut sched, 3, 300, exit(3));
        assert_eq!(finish(&sched, 2), (1_100, 1_100));
        assert_eq!(finish(&sched, 3), (400, 400));
        // The parent's wait reaps 2 at its 112, then 3 at its 125.
        commit(&mut sched, 1, 110, None);
        sched.work_span_note_reap(tid(1), 112, pid(1), pid(2));
        commit(&mut sched, 1, 120, None);
        assert_eq!(finish(&sched, 1), (1_108, 1_108), "2's exit, then 8 more");
        sched.work_span_note_reap(tid(1), 125, pid(1), pid(3));
        commit(&mut sched, 1, 170, None);
        assert_eq!(finish(&sched, 1), (1_158, 1_158));
        assert_eq!(report(&sched), (1_470, 1_158, 1_158));
    }

    /// A wait follows the child it reaped, whichever that is, and a wait that
    /// reaped nothing follows no exit (`waitpid(X)` while Y has exited).
    #[test]
    fn a_wait_follows_the_child_it_reaped() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 100, exit(2)); // Y exits at 100
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 1_000, exit(3)); // X exits at 1,000
        sched.work_span_note_reap(tid(1), 0, pid(1), pid(3));
        commit(&mut sched, 1, 500, None);
        assert_eq!(finish(&sched, 1), (1_500, 1_500), "X's exit, then 500");
        commit(&mut sched, 1, 600, None);
        assert_eq!(finish(&sched, 1), (1_600, 1_600), "Y is not reaped");
        assert!(sched.work_span.exited[&pid(1)].contains_key(&pid(2)));
        sched.consume_child_wait(pid(1), pid(2));
        assert!(!sched.work_span.exited[&pid(1)].contains_key(&pid(2)));
    }

    /// A read follows the last write to the same resource; reads do not
    /// order each other; a write follows the reads; a scheduler token orders
    /// nothing.
    #[test]
    fn conflicting_accesses_are_ordered() {
        let mut sched = family();
        let out = || ResourceID::Device(Device::ContainerStdout);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        for thread in [1, 2, 3] {
            commit(&mut sched, thread, 0, None);
        }
        commit(&mut sched, 2, 400, Some((out(), Permission::W)));
        commit(&mut sched, 3, 500, Some((out(), Permission::R)));
        commit(&mut sched, 1, 20, Some((out(), Permission::R)));
        assert_eq!(
            finish(&sched, 1),
            (400, 400),
            "after the write, not the read"
        );
        commit(&mut sched, 2, 401, Some((out(), Permission::W)));
        assert_eq!(finish(&sched, 2), (500, 500), "after both reads");
        commit(
            &mut sched,
            1,
            30,
            Some((ResourceID::FutexWait, Permission::RW)),
        );
        assert_eq!(finish(&sched, 1), (410, 410));
    }

    /// A futex waiter parked by the scheduler (`sleep_futex_waiter`, never
    /// through step 4) follows, in the upper span, every commit made while it
    /// was parked: A works 1,000 and wakes B, which works 1,000 more.
    #[test]
    fn a_parked_futex_waiter_follows_what_ran_while_it_slept() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let futex = FutexID::private(MmId::initial(tid(1)), 0x40);
        sched.sleep_futex_waiter(&tid(1), futex, None, u32::MAX, None);
        commit(&mut sched, 2, 1_000, None);
        let wait = request(1, Some((ResourceID::FutexWait, Permission::R)));
        sched.work_span_note_arrival(tid(1), 0, Arrival::Other);
        sched.work_span_commit(tid(1), Some(&wait));
        commit(&mut sched, 1, 1_000, None);
        assert_eq!(finish(&sched, 1), (1_000, 2_000));
        assert_eq!(sched.work_span.binding_waited[&WaitKind::Futex], 1);
    }

    /// A waker's `FUTEX_WAKE` is an RPC that does not end its turn, so the
    /// waiter can be granted before the waker commits again: the waiter
    /// follows the waker as far as it has run, not only its committed turns.
    #[test]
    fn a_woken_waiter_follows_its_wakers_uncommitted_work() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let futex = FutexID::private(MmId::initial(tid(1)), 0x40);
        sched.sleep_futex_waiter(&tid(1), futex, None, u32::MAX, None);
        // Thread 2 computes 1,000 and wakes thread 1 in an RPC.
        sched.work_span_note_arrival(tid(2), 1_000, Arrival::Other);
        let wait = request(1, Some((ResourceID::FutexWait, Permission::R)));
        sched.work_span_note_arrival(tid(1), 0, Arrival::Other);
        sched.work_span_commit(tid(1), Some(&wait));
        commit(&mut sched, 1, 1_000, None);
        assert_eq!(finish(&sched, 1), (1_000, 2_000));
    }

    /// A polled wait follows, in the upper span, what other threads did
    /// while it polled, and in the lower span nothing it did not reap.
    #[test]
    fn a_polled_wait_follows_what_ran_while_it_polled() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        sched.work_span_note_wait(tid(1), &poll(1));
        commit(&mut sched, 2, 700, None);
        sched.work_span_note_arrival(tid(1), 0, Arrival::Other);
        sched.work_span_commit(tid(1), Some(&poll(1)));
        assert_eq!(finish(&sched, 1), (0, 700));
        assert_eq!(sched.work_span.binding_waited[&WaitKind::Poll], 1);
    }

    /// A retry begins a wait when it arrives, not when the scheduler skips
    /// it: a thread queued ahead of the poller can be granted, work and park
    /// without committing (a futex wait) before the skip, and the poller
    /// still follows it.
    #[test]
    fn a_retry_follows_what_ran_before_its_skip() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        // Thread 1's first attempt found nothing; its retry arrives.
        sched.work_span_note_arrival(tid(1), 0, Arrival::Retry(&poll(1)));
        // Thread 2 is granted, works 1,000, and parks without committing.
        sched.work_span_note_arrival(tid(2), 1_000, Arrival::Other);
        // Only now does the scheduler skip the retry, then grant it.
        sched.work_span_note_wait(tid(1), &poll(1));
        sched.work_span_note_arrival(tid(1), 0, Arrival::Other);
        sched.work_span_commit(tid(1), Some(&poll(1)));
        assert_eq!(finish(&sched, 1), (0, 1_000));
    }

    /// A clone takes no turn of its own: the child starts where its creator
    /// had reached at the clone, sampled by the registration's request, not
    /// at the creator's latest commit.
    #[test]
    fn a_child_starts_at_its_creators_clone() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        // Thread 1 computes 5,000, then clones; the registration samples it.
        sched.work_span_note_arrival(tid(1), 5_000, Arrival::Other);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_arrival(tid(2), 5_000, Arrival::StartsThread);
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        commit(&mut sched, 2, 6_000, None);
        assert_eq!(finish(&sched, 2), (6_000, 6_000), "5,000, then 1,000");
    }

    /// The requests of a thread whose call runs outside the schedule arrive at
    /// host-timed moments: they raise no one and change nothing another
    /// thread reads, and its latest clock counts at its continuation's commit.
    #[test]
    fn requests_from_outside_the_schedule_keep_nothing() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let outside =
            ResourceID::BlockingExternalIO(crate::resources::ExternalOpId::new(tid(1), 1));
        commit(&mut sched, 1, 10, Some((outside, Permission::RW)));
        let futex = FutexID::private(MmId::initial(tid(1)), 0x40);
        sched.sleep_futex_waiter(&tid(2), futex, None, u32::MAX, None);
        sched.work_span_note_arrival(tid(1), 900, Arrival::Other);
        assert_eq!(sched.work_span.cpu_now[&tid(1)], 10);
        assert!(
            sched
                .work_span
                .waiting
                .get(&tid(2))
                .is_none_or(|(at, _)| *at == 0),
            "a host-timed request raises no one"
        );
        let resume =
            ResourceID::BlockedExternalContinue(crate::resources::ExternalOpId::new(tid(1), 1));
        commit(&mut sched, 1, 950, Some((resume, Permission::RW)));
        assert_eq!(finish(&sched, 1), (950, 950));
        assert!(sched.work_span.cpu_outside.is_empty());
    }

    /// A registered child's `StartNewThread` can arrive after other threads
    /// have taken turns: it sets the baseline and raises no one.
    #[test]
    fn a_childs_late_start_raises_no_one() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 1, 5_000, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        let futex = FutexID::private(MmId::initial(tid(1)), 0x40);
        commit(&mut sched, 3, 0, None);
        sched.sleep_futex_waiter(&tid(3), futex, None, u32::MAX, None);
        commit(&mut sched, 1, 5_100, None);
        let before = sched.work_span.waiting[&tid(3)].0;
        sched.work_span_note_arrival(tid(2), 5_000, Arrival::StartsThread);
        assert_eq!(sched.work_span.waiting[&tid(3)].0, before);
        assert_eq!(sched.work_span.cpu_committed[&tid(2)], 5_000);
        assert!(!sched.work_span.cpu_now.contains_key(&tid(2)));
    }

    /// A thread's requests after its `Exit` commits (its deregistration, its
    /// robust-list wakes) arrive at host-timed moments: they keep nothing.
    #[test]
    fn requests_after_an_exit_keep_nothing() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 10_000, exit(2));
        sched.committed_exits.insert(tid(2));
        let futex = FutexID::private(MmId::initial(tid(1)), 0x40);
        sched.sleep_futex_waiter(&tid(1), futex, None, u32::MAX, None);
        commit(&mut sched, 3, 0, None);
        let before = sched.work_span.waiting[&tid(1)].0;
        sched.work_span_note_arrival(tid(2), 10_500, Arrival::Other);
        assert_eq!(sched.work_span.waiting[&tid(1)].0, before);
        assert_eq!(sched.work_span.cpu_now[&tid(2)], 10_000);
    }

    /// A vfork: the parent is outside the schedule from its `BlockingVfork`
    /// commit, the registration comes from the child, and the child starts at
    /// that commit.
    #[test]
    fn a_vfork_child_starts_at_its_parents_vfork_commit() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        let vfork = ResourceID::BlockingVfork(crate::resources::ExternalOpId::new(tid(1), 1));
        commit(&mut sched, 1, 3_000, Some((vfork, Permission::RW)));
        sched.next_turns.remove(&tid(2));
        sched.work_span_note_arrival(tid(2), 3_000, Arrival::Other);
        register(&mut sched, 2);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_arrival(tid(2), 3_000, Arrival::StartsThread);
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        commit(&mut sched, 2, 3_400, None);
        assert_eq!(finish(&sched, 2), (3_400, 3_400));
    }

    /// A thread killed while parked (its process's `exit_group`, an exec)
    /// never commits again: the work its last request sampled is counted at
    /// its retirement, once, and the span reaches it.
    #[test]
    fn retirement_counts_work_sampled_since_the_last_commit() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        // Thread 2 computes 700 and parks; its request sampled the 700.
        sched.work_span_note_arrival(tid(2), 700, Arrival::Other);
        sched.work_span_retire_thread(tid(2), pid(2));
        sched.work_span_retire_thread(tid(2), pid(2));
        let (work, lo, hi) = report(&sched);
        assert_eq!(work, 700, "counted once");
        assert_eq!((lo, hi), (700, 700));
    }

    /// A process is reaped only after all its threads are gone: its exit
    /// record joins a leader that computed and exited early, not only the
    /// cheap last thread.
    #[test]
    fn a_process_exit_joins_every_thread_of_the_process() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        // Process 2's worker, thread 8.
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(2), tid(8), false, false, 0);
        register(&mut sched, 8);
        sched.work_span_note_spawn(tid(2), tid(8));
        commit(&mut sched, 8, 0, None);
        // The leader computes 5,000 and exits alone (SYS_exit), then retires.
        let leader_exit = Some((
            ResourceID::Exit {
                group: false,
                process: pid(2),
                mm: MmId::initial(tid(2)),
            },
            Permission::W,
        ));
        commit(&mut sched, 2, 5_000, leader_exit.clone());
        assert!(!sched.work_span.exited.contains_key(&pid(1)), "not the end");
        sched.next_turns.remove(&tid(2));
        sched.work_span_retire_thread(tid(2), pid(2));
        // The worker exits cheaply, ending the process.
        commit(&mut sched, 8, 10, leader_exit);
        sched.logically_exited_processes.insert(pid(2));
        assert_eq!(sched.work_span.exited[&pid(1)][&pid(2)].lo, 5_000);
        // The parent reaps it at once and computes 100.
        sched.work_span_note_reap(tid(1), 0, pid(1), pid(2));
        commit(&mut sched, 1, 100, None);
        assert_eq!(finish(&sched, 1), (5_100, 5_100));
    }

    /// A subreaper that adopts an already exited orphan reaps it after its
    /// exit: the record moves to the adopter, and when it was dropped past
    /// the cap, the former parent's latest child exit does, in the upper span.
    #[test]
    fn an_adopted_zombie_keeps_its_exit_record() {
        for evicted in [false, true] {
            let mut sched = family();
            commit(&mut sched, 1, 0, None);
            sched.work_span_note_spawn(tid(1), tid(2));
            commit(&mut sched, 2, 0, None);
            // Process 2's child, process 9, computes 4,000 and exits.
            sched.thread_tree.add_child_with_wait_metadata(
                tid(2),
                tid(9),
                true,
                false,
                libc::SIGCHLD,
            );
            register(&mut sched, 9);
            sched.work_span_note_spawn(tid(2), tid(9));
            commit(&mut sched, 9, 0, None);
            commit(&mut sched, 9, 4_000, exit(9));
            if evicted {
                sched.work_span.exited.get_mut(&pid(2)).unwrap().clear();
            }
            // Process 2 exits cheaply without reaping it; 1 adopts it.
            commit(&mut sched, 2, 10, exit(2));
            sched.work_span_retire_thread(tid(2), pid(2));
            sched.work_span_adopt(pid(2), pid(1), pid(9));
            sched.work_span_parent_gone(pid(2));
            assert!(!sched.work_span.exited.contains_key(&pid(2)));
            // The subreaper reaps the orphan at once and computes 10.
            sched.work_span_note_reap(tid(1), 0, pid(1), pid(9));
            commit(&mut sched, 1, 10, None);
            let expected = if evicted { (10, 4_010) } else { (4_010, 4_010) };
            assert_eq!(finish(&sched, 1), expected, "evicted {evicted}");
        }
    }

    /// The same adoption through the scheduler's own retirement of the
    /// intermediate parent (`logically_kill_thread`, which places its orphans
    /// with `reparent_orphans` and then drops its state as a parent).
    #[test]
    fn a_subreaper_adopts_a_zombie_through_the_scheduler() {
        let mut sched = family();
        sched.thread_tree.set_child_subreaper(pid(1), true);
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(2), tid(9), true, false, libc::SIGCHLD);
        register(&mut sched, 9);
        sched.work_span_note_spawn(tid(2), tid(9));
        commit(&mut sched, 9, 0, None);
        commit(&mut sched, 9, 4_000, exit(9));
        sched.logically_kill_thread(&tid(9), &pid(9), MmId::initial(tid(9)));
        commit(&mut sched, 2, 10, exit(2));
        sched.logically_kill_thread(&tid(2), &pid(2), MmId::initial(tid(2)));
        assert_eq!(sched.thread_tree.parent_process(&pid(9)), Some(pid(1)));
        assert!(!sched.work_span.exited.contains_key(&pid(2)));
        assert_eq!(sched.work_span.exited[&pid(1)][&pid(9)].lo, 4_000);
    }

    /// A raw clone child (its own process in the thread tree, reported under
    /// its creator's pid) frees its process's completion when it leaves,
    /// although its creator lives on.
    #[test]
    fn a_raw_clone_childs_completion_is_freed() {
        let mut sched = family();
        commit(&mut sched, 2, 0, None);
        sched
            .thread_tree
            .add_child_with_wait_metadata(tid(2), tid(9), true, false, libc::SIGUSR1);
        register(&mut sched, 9);
        sched.work_span_note_spawn(tid(2), tid(9));
        commit(&mut sched, 9, 0, None);
        commit(&mut sched, 9, 50, exit(9));
        assert!(sched.work_span.process_done.contains_key(&pid(9)));
        sched.logically_kill_thread(&tid(9), &pid(2), MmId::initial(tid(9)));
        assert!(!sched.work_span.process_done.contains_key(&pid(9)));
    }

    /// An orphan adopted by the container's init has no guest reaper: its
    /// exit creates no record under its dead parent, so repeated
    /// parent-before-child exits leave nothing behind.
    #[test]
    fn init_orphans_leave_no_parent_records() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        for n in 0..50 {
            let (parent, child) = (100 + 2 * n, 101 + 2 * n);
            for (creator, thread) in [(1, parent), (parent, child)] {
                sched.thread_tree.add_child_with_wait_metadata(
                    tid(creator),
                    tid(thread),
                    true,
                    false,
                    libc::SIGCHLD,
                );
                register(&mut sched, thread);
                sched.work_span_note_spawn(tid(creator), tid(thread));
                commit(&mut sched, thread, 0, None);
            }
            // The parent exits first; its child goes to init.
            commit(&mut sched, parent, 1, exit(parent));
            sched.work_span_retire_thread(tid(parent), pid(parent));
            sched.thread_tree.orphan_to_init(pid(child));
            sched.work_span_parent_gone(pid(parent));
            commit(&mut sched, child, 1, exit(child));
            sched.work_span_retire_thread(tid(child), pid(child));
            sched.work_span_parent_gone(pid(child));
            assert!(!sched.work_span.exited.contains_key(&pid(parent)));
            assert!(!sched.work_span.latest_child_exit.contains_key(&pid(parent)));
        }
        assert!(sched.work_span.process_done.is_empty());
    }

    /// An exec transfer's clock is the former thread's: it keeps nothing.
    #[test]
    fn an_exec_transfer_keeps_nothing() {
        let mut sched = family();
        commit(&mut sched, 2, 0, None);
        sched.work_span_note_arrival(tid(2), 999, Arrival::ExecTransfer);
        assert_eq!(sched.work_span.cpu_now[&tid(2)], 0);
    }

    /// A sleep and other scheduler-only waits order nothing.
    #[test]
    fn a_sleep_waits_on_no_one() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let sleep = ResourceID::SleepUntil(LogicalTime::from_nanos(1));
        sched.work_span_note_wait(tid(1), &request(1, Some((sleep, Permission::R))));
        commit(&mut sched, 2, 2_000, None);
        commit(&mut sched, 1, 10, None);
        assert_eq!(finish(&sched, 1), (10, 10));
    }

    /// A call that ran outside the schedule (a blocking external call)
    /// follows what other threads did meanwhile, in the upper span.
    #[test]
    fn a_call_outside_the_schedule_follows_what_ran_meanwhile() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        commit(
            &mut sched,
            1,
            10,
            Some((
                ResourceID::BlockingExternalIO(crate::resources::ExternalOpId::new(tid(1), 1)),
                Permission::RW,
            )),
        );
        commit(&mut sched, 2, 900, None);
        commit(&mut sched, 1, 20, None);
        assert_eq!(finish(&sched, 1), (20, 900));
        assert_eq!(sched.work_span.binding_waited[&WaitKind::Outside], 1);
    }

    /// A child's clock starts at its creator's: that inherited time, which its
    /// first request (`StartNewThread`) carries, is not the child's work.
    #[test]
    fn a_childs_inherited_clock_is_not_its_work() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 1, 5_000, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_arrival(tid(2), 5_000, Arrival::StartsThread);
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        commit(&mut sched, 2, 5_050, None);
        assert_eq!(
            report(&sched).0,
            5_050,
            "the parent's 5,000 and the child's 50"
        );
        assert_eq!(finish(&sched, 2), (5_050, 5_050));
    }

    /// A child's first request can arrive before its creator's registration
    /// of it returns (host-timed): its baseline is its inherited clock either
    /// way, so its work is the same.
    #[test]
    fn a_childs_baseline_does_not_depend_on_registration_order() {
        for registered_first in [true, false] {
            let mut sched = family();
            commit(&mut sched, 1, 0, None);
            commit(&mut sched, 1, 5_000, None);
            sched.next_turns.remove(&tid(2));
            if registered_first {
                register(&mut sched, 2);
                sched.work_span_note_spawn(tid(1), tid(2));
            }
            sched.work_span_note_arrival(tid(2), 5_000, Arrival::StartsThread);
            if !registered_first {
                register(&mut sched, 2);
                sched.work_span_note_spawn(tid(1), tid(2));
            }
            sched.work_span_commit(tid(2), Some(&request(2, None)));
            commit(&mut sched, 2, 5_050, None);
            assert_eq!(
                report(&sched).0,
                5_050,
                "registered first {registered_first}"
            );
        }
    }

    /// A commit without any sample and without a baseline invents none: it
    /// weighs nothing, and the thread's next request sets the baseline.
    #[test]
    fn a_commit_without_a_sample_invents_no_baseline() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        assert!(!sched.work_span.cpu_committed.contains_key(&tid(2)));
        // The child's first sample carries its inherited 4,000.
        commit(&mut sched, 2, 4_000, None);
        commit(&mut sched, 2, 4_030, None);
        assert_eq!(report(&sched).0, 30);
    }

    /// A non-leader that execs goes on as the leader with its own timeline.
    #[test]
    fn an_exec_keeps_the_callers_timeline() {
        let mut sched = family();
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 500, None);
        register(&mut sched, 7);
        sched.work_span_transfer_thread(tid(2), tid(7));
        assert!(!sched.work_span.finish.contains_key(&tid(2)));
        commit(&mut sched, 7, 520, None);
        assert_eq!(finish(&sched, 7), (520, 520));
    }

    /// A retired thread's state goes, and with its process's last thread its
    /// completion; its state as a parent goes once its orphans are placed. A
    /// request from a retired thread keeps nothing.
    #[test]
    fn retirement_frees_a_threads_state() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 10, exit(2));
        assert!(!sched.work_span.exited.is_empty());
        sched.work_span_retire_thread(tid(2), pid(2));
        sched.work_span_retire_thread(tid(1), pid(1));
        assert_eq!(
            sched.work_span_report().unwrap().threads,
            2,
            "retired threads still count"
        );
        // A parent's records stay until its orphans are placed, which may
        // move them to an adopter.
        assert!(sched.work_span.exited.contains_key(&pid(1)));
        sched.work_span_parent_gone(pid(2));
        sched.work_span_parent_gone(pid(1));
        let state = &sched.work_span;
        assert!(state.finish.is_empty() && state.cpu_now.is_empty());
        assert!(state.cpu_committed.is_empty() && state.spawned.is_empty());
        assert!(state.cpu_outside.is_empty() && state.process_done.is_empty());
        assert!(state.exited.is_empty() && state.latest_child_exit.is_empty());
        assert!(state.pending_access.is_empty() && state.waiting.is_empty());
        sched.next_turns.remove(&tid(2));
        sched.work_span_note_arrival(tid(2), 50, Arrival::Other);
        assert!(sched.work_span.cpu_now.is_empty());
        assert!(sched.work_span.cpu_committed.is_empty());
        assert_eq!(sched.work_span_report().unwrap().threads, 2);
    }

    /// A signal delivery follows, in the upper span, whatever any thread had
    /// reached, committed or not, even when its receiver slept rather than
    /// waited: A computes 1,000 and signals B, which slept, then computes
    /// 1,000.
    #[test]
    fn a_signal_delivery_follows_every_thread_in_the_upper_span() {
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        let sleep = ResourceID::SleepUntil(LogicalTime::from_nanos(1));
        commit(&mut sched, 1, 0, Some((sleep, Permission::R)));
        sched.work_span_note_arrival(tid(2), 1_000, Arrival::Other);
        commit(
            &mut sched,
            1,
            0,
            Some((
                ResourceID::InboundSignal(SigWrapper(libc::SIGUSR1)),
                Permission::W,
            )),
        );
        commit(&mut sched, 1, 1_000, None);
        assert_eq!(finish(&sched, 1), (1_000, 2_000));
        assert_eq!(sched.work_span.binding_hi[&EdgeKind::Signal], 1);
    }

    /// Past `MAX_EXITED_PER_PARENT` the oldest exit goes; a reap of exactly
    /// that child, ready at its first attempt (no wait), follows the
    /// parent's latest child exit in the upper span.
    #[test]
    fn a_reap_of_an_evicted_exit_follows_the_latest_child_exit() {
        let mut sched = family();
        sched.work_span.exited_cap = 3;
        commit(&mut sched, 1, 0, None);
        let children = 4..7;
        // Child 2 works 1,000 and exits first; then a cap's worth more exit.
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 1_000, exit(2));
        for child in children.clone() {
            sched.thread_tree.add_child_with_wait_metadata(
                tid(1),
                tid(child),
                true,
                false,
                libc::SIGCHLD,
            );
            register(&mut sched, child);
            sched.work_span_note_spawn(tid(1), tid(child));
            commit(&mut sched, child, 0, None);
            commit(&mut sched, child, 0, exit(child));
        }
        assert!(!sched.work_span.exited[&pid(1)].contains_key(&pid(2)));
        assert_eq!(sched.work_span.exited[&pid(1)].len(), 3);
        sched.work_span_note_reap(tid(1), 0, pid(1), pid(2));
        commit(&mut sched, 1, 10, None);
        assert_eq!(finish(&sched, 1), (10, 1_010));
    }

    /// Past `MAX_RESOURCES` the table is cleared: the lower span loses those
    /// edges, and the upper span orders every later first access after the
    /// cleared table's latest time, so a forgotten write still orders it.
    #[test]
    fn the_resource_table_is_bounded_and_stays_sound() {
        let path = |n: usize| ResourceID::Path(PathBuf::from(format!("/p{n}")));
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 1, 1_000, Some((path(0), Permission::W)));
        for n in 1..=MAX_RESOURCES {
            commit(&mut sched, 1, 1_000, Some((path(n), Permission::W)));
        }
        assert_eq!(sched.work_span.resource_evictions, 1);
        assert!(sched.work_span.resources.len() <= 1);
        // Thread 2 reads /p0, which thread 1 wrote at 1,000 before the clear.
        commit(&mut sched, 2, 7, Some((path(0), Permission::R)));
        assert_eq!(
            finish(&sched, 2),
            (7, 1_000),
            "the floor, in the upper span only"
        );
        // An independent second reader also follows the forgotten write.
        sched.work_span_note_spawn(tid(9), tid(3));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 5, Some((path(0), Permission::R)));
        commit(&mut sched, 3, 1_005, None);
        assert_eq!(finish(&sched, 3), (1_005, 2_000));
    }

    /// A resource access runs after its grant: a conflicting access follows
    /// the accessor as far as it has run until it commits again, then that
    /// commit's time.
    #[test]
    fn a_resource_access_includes_its_own_syscall_time() {
        let out = || ResourceID::Device(Device::ContainerStdout);
        let mut sched = family();
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 1, 0, None);
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 100, Some((out(), Permission::W)));
        // Thread 2's write syscall costs 50 before its next request.
        sched.work_span_note_arrival(tid(2), 150, Arrival::Other);
        commit(&mut sched, 1, 1, Some((out(), Permission::R)));
        assert_eq!(
            finish(&sched, 1),
            (100, 150),
            "while thread 2 has not committed"
        );
        sched.work_span_commit(tid(2), Some(&request(2, None)));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 1, Some((out(), Permission::R)));
        assert_eq!(finish(&sched, 3), (100, 150), "after it did");
    }

    /// The exits a parent can still reap are the zombies the scheduler holds:
    /// one reaped automatically is forgotten when the next child exits.
    #[test]
    fn automatically_reaped_exits_are_forgotten() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        sched.work_span_note_spawn(tid(1), tid(3));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 10, exit(2));
        sched.autoreaped_exits.insert(pid(2));
        commit(&mut sched, 3, 0, None);
        commit(&mut sched, 3, 10, exit(3));
        let kept: Vec<DetPid> = sched.work_span.exited[&pid(1)].keys().copied().collect();
        assert_eq!(kept, vec![pid(3)]);
    }

    /// A reap without its exit record (dropped or pruned) follows the
    /// parent's latest child exit in the upper span.
    #[test]
    fn a_reap_without_a_record_follows_the_latest_child_exit() {
        let mut sched = family();
        commit(&mut sched, 1, 0, None);
        sched.work_span_note_spawn(tid(1), tid(2));
        commit(&mut sched, 2, 0, None);
        commit(&mut sched, 2, 300, exit(2));
        sched.work_span.exited.clear();
        sched.work_span_note_reap(tid(1), 0, pid(1), pid(2));
        commit(&mut sched, 1, 5, None);
        assert_eq!(finish(&sched, 1), (5, 305));
    }

    /// A request with several resources takes the most conservative wait.
    #[test]
    fn a_request_takes_its_most_conservative_wait() {
        let mut both = poll(1);
        both.insert(
            ResourceID::InboundSignal(SigWrapper(libc::SIGUSR1)),
            Permission::RW,
        );
        assert_eq!(wait_kind(&both), Some(WaitKind::Signal));
        let mut sleep = poll(1);
        sleep.insert(
            ResourceID::SleepUntil(LogicalTime::from_nanos(1)),
            Permission::R,
        );
        assert_eq!(wait_kind(&sleep), Some(WaitKind::Poll));
        assert_eq!(wait_kind(&request(1, None)), None);
    }

    /// Every report lists what it does not model, so a reader knows the
    /// lower span can be optimistic.
    #[test]
    fn the_report_lists_what_it_does_not_model() {
        let report = family().work_span_report().unwrap();
        assert_eq!(report.unmodelled, UNMODELLED.to_vec());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["unmodelled"][0], "wait_causes");
    }

    /// Off by default: nothing is tracked and there is no report.
    #[test]
    fn off_by_default() {
        let mut sched = Scheduler::new(&Config::default());
        assert!(!sched.work_span.enabled);
        sched.work_span_note_arrival(tid(1), 10, Arrival::StartsThread);
        sched.work_span_commit(tid(1), None);
        assert!(sched.work_span.cpu_now.is_empty());
        assert!(sched.work_span.cpu_committed.is_empty());
        assert!(sched.work_span_report().is_none());
    }
}
