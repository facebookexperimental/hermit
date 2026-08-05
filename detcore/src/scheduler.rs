/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic scheduling algorithm.

mod replayer;
pub mod runqueue;
pub mod timed_waiters;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Write;
use std::iter::Peekable;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::vec::IntoIter;

use detcore_model::happens_before::HappensBeforeProgram;
use detcore_model::happens_before::Position;
use detcore_model::happens_before::Strength;
use detcore_model::happens_before::ThreadRef;
use detcore_model::summary::RunSummary;
use detcore_model::summary::TimesliceStats;
use nix::sys::signal;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use rand::RngExt as _;
use rand::SeedableRng;
use rand::seq::IndexedRandom;
use rand::seq::SliceRandom;
use rand_pcg::Pcg64Mcg;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
pub use runqueue::DEFAULT_PRIORITY;
use runqueue::LAST_PRIORITY;
use runqueue::PrioritizedOrder;
pub use runqueue::Priority;
use runqueue::REPLAY_DEFERRED_PRIORITY;
use runqueue::REPLAY_FOREGROUND_PRIORITY;
use runqueue::RunQueue;
pub use runqueue::entropy_to_priority;
use serde::Deserialize;
use serde::Serialize;
use timed_waiters::TimedEvent;
use timed_waiters::TimedEvents;
use tracing::Level;
use tracing::debug;
use tracing::enabled;
use tracing::info;
use tracing::trace;

use crate::config::Config;
use crate::config::RunsPostFork;
use crate::detlog_debug;
use crate::ivar::Ivar;
use crate::preemptions::PreemptionWriter;
use crate::preemptions::read_trace;
use crate::resources::ExternalOpId;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::resources::SABRE_INTERNAL_PIPE_IO_FYI;
use crate::resources::SABRE_LOOPBACK_POLL_YIELD_FYI;
use crate::scheduler::replayer::StopReason;
use crate::scheduler::replayer::events_consistent;
use crate::scheduler::replayer::events_match;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::FutexID;
use crate::types::GlobalTime;
use crate::types::LogicalTime;
use crate::types::MmId;
use crate::types::SchedEvent;
use crate::types::SigWrapper;
use crate::types::SyscallPhase;
use crate::util::truncated;

/// Unique identifier for an action.
pub type ActionID = u64;

/// A representation of side effects that are happening, or could be happening, right now
/// in the background.
#[derive(Debug, Clone)]
pub struct Action {
    /// Id for the action
    #[allow(dead_code)]
    pub action_id: ActionID,

    /// The action's side effects are completed.
    #[allow(dead_code)]
    pub completion: Ivar<()>,

    /// Which action gets the lock after me.
    #[allow(dead_code)]
    pub successors: HashMap<ResourceID, ActionID>,
}

/// The response from the scheduler that wakes back up a guest thread after a request.
#[derive(Debug, Clone)]
pub enum SchedResponse {
    /// Keep running.
    Go(Option<SchedValue>),

    /// The guest was interupted by a signal while waiting on the scheduler, and will now execute
    /// the handler.
    Signaled(),
    // TODO: Time to exit, or an exit is already under way
    // Exit,
}

#[derive(Debug, Clone, PartialOrd, PartialEq, Eq, Serialize, Deserialize)]
/// A value that the scheduler returns to the guest when resuming.  This is weakly typed in that it
/// is only relevant to certain scheduler requests, and its meaning is dependent on what was
/// requested of the scheduler.
///
/// It can be used to have the scheduler EMULATE behaviors (syscalls) that would normally happen in
/// the guest. The first application for this is futexes.
pub enum SchedValue {
    /// The action timed out while waiting on the scheduler.
    TimeOut,
    // TODO(T137799529) make this more strongly typed, an enum for different scenarios:
    Value(u64),
}

/// A single interaction between a guest and the scheduler: first, request resourcees, followed
/// by an ACK to "go ahead".  This thread record includes a bit of thread metadata.
#[derive(Debug, Clone)]
pub struct ThreadNextTurn {
    /// The logical Tid of the guest thread.
    pub dettid: DetTid,
    /// Address of where the child thread Tid will be cleared if CLEARTID was set on clone.
    pub child_tid_addr: usize,
    /// Request from the thread to the scheduler.
    pub req: Ivar<SchedRequest>,
    /// A place for the response when that request is fulfilled.
    pub resp: Ivar<SchedResponse>,
}

/// State needed to replace a process's scheduler identity after successful exec.
pub(crate) struct ExecReconnect {
    pub caller: DetTid,
    pub new_leader: DetTid,
    pub detpid: DetPid,
    pub pre_exec_mm: MmId,
    pub post_exec_mm: MmId,
    pub child_tid_addr: usize,
    pub reconnect_priority: Option<Priority>,
}

/// Request for resources when the thread next parks.
/// OR the thread might "park" because it's really exited.
pub type SchedRequest = Result<Resources, ThreadExited>;

/// Unit value to signal that the thread has exited.
// TODO: could put an exit status here.
#[derive(Debug, Clone)]
pub struct ThreadExited;

/// A thread waiting on a futex, including the bitset accepted by wake operations.
#[derive(Debug, Clone)]
pub struct FutexWaiter {
    dettid: DetTid,
    response: Ivar<SchedResponse>,
    bitset: u32,
}

/// Deterministically pick one element from `choices` using the supplied PRNG,
/// returning `None` for an empty slice. This is the single random-selection
/// primitive used by the targeted-chaos scheduling points, so their choices stay
/// reproducible under a fixed `--fuzz-seed`.
fn chaos_pick<T: Copy>(prng: &mut Pcg64Mcg, choices: &[T]) -> Option<T> {
    choices.choose(prng).copied()
}

fn take_matching_futex_waiters(waiters: &mut Vec<FutexWaiter>, wake_mask: u32) -> Vec<FutexWaiter> {
    let (matching, remaining) = std::mem::take(waiters)
        .into_iter()
        .partition(|waiter| waiter.bitset & wake_mask != 0);
    *waiters = remaining;
    matching
}

/// Actions that are blocked on another internal action of the guest, such as a pipe communication,
/// or are blocked on external conditions such as a network request.  These cannot consume a logical
/// turn until a matching unblocking action is ready.
///
/// This structure will NOT include blocking operations that are implemented via polling.
/// See NOTE [Blocking Syscalls via Internal Polling] in this folder.
#[derive(Debug, Clone, Default)]
pub struct BlockedPool {
    /// BLOCKED futex transactions, waiting for wakers. Multiple threads may be blocked on
    /// the same futex.
    ///
    /// INVARIANT: because Futexes aren't currently modeled with `ResourceID`, a thread
    /// waiting on a futex will have a request filled in `next_turns` but for zero resources.
    pub futex_waiters: HashMap<FutexID, Vec<FutexWaiter>>,

    /// Futex waiters whose deadlines expired and must receive `ETIMEDOUT` when scheduled.
    /// Timed-out waiters are removed from `futex_waiters` before entering the run queue.
    pub timed_out_futex_waiters: HashSet<DetTid>,

    /// Threads whose next event is waiting on a point in time to proceed.
    ///
    /// This is sorted by soonest time of occurrence.
    /// NOTE: futex waiters will ALSO appear in here if they have timeouts.
    pub timed_waiters: TimedEvents,

    /// Blockers on external IO that are in the middle of executing (or have finished) and
    /// are waiting for permission from the scheduler to resume.
    ///
    /// The protocol here is that the `(request,response)` pair (in `next_turns`) for
    /// threads in `external_io_blockers` will have the request filled in with an
    /// `BlockedExternalContinue` request when the thread is past its blocking action and
    /// waiting for permission to resume. A failed operation governed by `BlockingVfork` instead
    /// reports `VforkFailed`, which follows the same re-admission path after cancelling its
    /// barrier. The request will stay empty while the thread is doing the blocking action. This is
    /// different than the normal relationship
    pub external_io_blockers: BTreeMap<DetTid, ExternalOpId>,

    /// Parents parked awaiting deterministic delivery of a host-async `SIGCHLD`.
    ///
    /// When a guest child process exits, the kernel raises `SIGCHLD` on the
    /// parent at a moment decided purely by host timing. If the resulting
    /// `InboundSignal` turn is committed as soon as it arrives, its position
    /// races whatever guest work was already runnable -- classically a `make -jN`
    /// jobserver `pselect6` continuation -- and `--strict --verify` diverges.
    ///
    /// Instead the parent is parked here, out of the run queue, and re-admitted
    /// by `step2e_process_signal_deferred` only once no ordinary (non-poller)
    /// guest work remains: the same deterministic-work-first policy that governs
    /// `external_io_blockers`. The physical signal has already been delivered by
    /// the kernel, so the handler's `wait4`/`waitpid` still reaps a real host
    /// zombie and no synthetic signal is ever generated.
    pub sigchld_deferred: BTreeSet<DetTid>,

    /// Deferred `SIGCHLD` parents that `step2e_process_signal_deferred` has
    /// re-admitted to the run queue. Their `InboundSignal` turn must now be
    /// granted rather than deferred again on the turn the scheduler selects them.
    pub sigchld_ready: BTreeSet<DetTid>,
}

impl BlockedPool {
    /// Returns true if there are NO blocked threads waiting outside the run-queue.
    fn is_empty(&self) -> bool {
        self.no_futex_waiters()
            && self.timed_waiters.is_empty()
            && self.external_io_blockers.is_empty()
            && self.sigchld_deferred.is_empty()
    }

    /// True if there are no runnable threads, and the only blocked ones are externally-blocked.
    fn only_external_blocked(&self) -> bool {
        self.no_futex_waiters()
            && self.timed_waiters.is_empty()
            && !self.external_io_blockers.is_empty()
    }

    /// Returns true if there are zero threads blocked on futexes.
    fn no_futex_waiters(&self) -> bool {
        self.futex_waiters.iter().all(|(_, v)| v.is_empty())
    }
}

/// Record the expectations about requests to continue after blocking IO.
fn external_continue_id(req: &Resources) -> ExternalOpId {
    assert_eq!(req.resources.len(), 1);
    let rsrc = req.resources.iter().next().unwrap().0;
    match rsrc {
        ResourceID::BlockedExternalContinue(op_id) | ResourceID::VforkFailed(op_id) => *op_id,
        other => panic!("expected external continue request, got {other:?}"),
    }
}

/// Runtime state for enforcing a [`HappensBeforeProgram`] inside the scheduler.
///
/// The scheduler holds each edge's AFTER anchor -- removing that thread from the
/// run queue -- until the edge's BEFORE anchor has *fired*, so an authored
/// partial order deterministically reproduces a known race instead of relying on
/// a seed lottery. An anchor "fires" when its thread is granted passage past the
/// corresponding checkpoint (see [`Scheduler::hb_checkpoint`]).
///
/// Only [`Position::SyscallCount`] anchors are enforced in this milestone. Other
/// position kinds are retained for diagnostics but never fire; [`HbRuntime::new`]
/// warns about them so a run never silently ignores an ordering constraint.
#[derive(Debug)]
struct HbRuntime {
    /// The validated, normalized program (anchors indexed by name, plus edges).
    program: HappensBeforeProgram,
    /// Names of anchors that have fired. Monotonic: an anchor fires at most once,
    /// when its thread is first granted passage past it.
    fired: BTreeSet<String>,
    /// Threads currently parked at an AFTER anchor, out of the run queue, awaiting
    /// their gating BEFORE anchor(s). A `BTreeSet` keeps re-admission order
    /// deterministic.
    parked: BTreeSet<DetTid>,
    /// Threads observed at creation time, in deterministic spawn order, so an
    /// anchor addressed by `spawn_ordinal` resolves to a concrete `DetTid`.
    /// Index 0 is the root thread; index N (1-based) is the Nth spawned child,
    /// matching [`ThreadRef::spawn_ordinal`] semantics.
    spawn_order: Vec<DetTid>,
    /// Set when a newly fired anchor may have opened a parked thread's gate, so
    /// [`Scheduler::hb_flush_wakes`] re-admits parked threads at the next
    /// `step3` boundary. Re-admission is *deferred* to that boundary because it
    /// pushes to the run queue, which is illegal while a `tentative_pop`
    /// selection is in progress (as it is inside `block_for_one_resource`,
    /// where anchors fire).
    wake_pending: bool,
}

impl HbRuntime {
    /// Build runtime state from a normalized program, warning about any anchor
    /// whose position kind this milestone does not enforce.
    fn new(program: HappensBeforeProgram) -> Self {
        for anchor in program.unenforced_positions() {
            tracing::warn!(
                "[happens-before] anchor {} uses position '{}', which the scheduler does not yet \
                 enforce (only 'after N syscalls' is enforced); this ordering constraint will NOT \
                 be applied",
                anchor.name,
                anchor.position,
            );
        }
        Self {
            program,
            fired: BTreeSet::new(),
            parked: BTreeSet::new(),
            spawn_order: Vec::new(),
            wake_pending: false,
        }
    }

    /// Record a thread at creation time for `spawn_ordinal` resolution. Idempotent
    /// and cheap; the root thread lands at index 0, the Nth child at index N.
    fn note_spawn(&mut self, dettid: DetTid) {
        if !self.spawn_order.contains(&dettid) {
            self.spawn_order.push(dettid);
        }
    }

    /// True when `tref` resolves to `dettid`, by explicit `DetTid` or by
    /// `spawn_ordinal` against the observed spawn order.
    fn thread_matches(&self, tref: &ThreadRef, dettid: DetTid) -> bool {
        if let Some(d) = tref.dettid {
            return d == dettid;
        }
        if let Some(ord) = tref.spawn_ordinal {
            return self.spawn_order.get(ord as usize).copied() == Some(dettid);
        }
        false
    }

    /// Names of anchors on `dettid` whose enforced position is exactly
    /// `SyscallCount(count)`.
    fn anchors_at_syscall(&self, dettid: DetTid, count: u64) -> Vec<String> {
        self.program
            .anchors
            .values()
            .filter(|a| {
                matches!(a.position, Position::SyscallCount(n) if n == count)
                    && self.thread_matches(&a.thread, dettid)
            })
            .map(|a| a.name.clone())
            .collect()
    }

    /// True when anchor `name` is the AFTER endpoint of a Hard edge whose BEFORE
    /// endpoint has not yet fired -- i.e. a thread reaching `name` must be held.
    fn anchor_blocked(&self, name: &str) -> bool {
        self.program.edges.iter().any(|e| {
            e.after == name && e.strength == Strength::Hard && !self.fired.contains(&e.before)
        })
    }
}

/// Which end of a thread's priority band a run-queue admission targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitSide {
    /// Run before an equal-priority peer (`runqueue_push_front`).
    Front,
    /// Ordinary tail admission (`runqueue_push_back`).
    Back,
}

/// How a run-queue admission's side is determined.
///
/// The side is *resolved* — not merely applied — at the deterministic drain
/// ([`Scheduler::drain_pending_run_queue_admissions`]). Buffering the *intent*
/// rather than an already-chosen `AdmitSide` is what keeps the admission a pure
/// function of deterministic scheduler state: any PRNG draw that picks the side
/// (`RunsPostFork::Random`) is consumed at the drain, in canonical `DetTid`
/// order, instead of in host RPC / lock-acquisition order at the handler. See
/// [`Scheduler::admit_to_run_queue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitIntent {
    /// The side is fixed regardless of scheduler state; consumes no PRNG.
    Fixed(AdmitSide),
    /// The side follows the post-fork policy; `RunsPostFork::Random` draws from
    /// the scheduler PRNG at resolution time.
    PostFork(RunsPostFork),
}

/// Why a raw TID must be removed from the physical run queue at the next
/// deterministic drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovalDisposition {
    /// The current thread incarnation is gone. Any admission recorded for the
    /// same raw TID is stale and must be cancelled.
    Retire,
    /// Linux nonleader exec destroyed the old process leader and reassigned
    /// its raw TID to the caller's replacement image. Remove the old physical
    /// queue slot, but preserve the one causally paired replacement admission.
    ReplaceThenAdmit,
}

/// The state for the deterministic scheduler.
#[derive(Debug)]
pub struct Scheduler {
    /// Monotonically count upwards.
    pub turn: u64,

    /// The queue of logically UNBLOCKED guest threads waiting for a turn.  After a new
    /// thread is created, it should always have an entry in here, but it goes to the end
    /// of the line after its turn. Unblocked threads are dequed in priority order, then
    /// round-robin within a priority level.
    /// NB: Polling threads are considered unblocked, and their polling intervals are managed by the RunQueue
    pub run_queue: RunQueue,

    /// Stores the communication endpoints for rendevous with each guest on its next turn.
    /// When the thread parks it provides its request for resources, and waits for a
    /// response.  After a new thread is created, it should always have an entry in here.
    ///
    /// Parked threads are READY, waiting only for the scheduler.
    ///
    /// (N.B.  This is a BTreeMap because we iterate over it, printing the
    /// contents, and BTreeMap gives us a predictable order, unlike HashMap.)
    pub next_turns: BTreeMap<DetTid, ThreadNextTurn>,

    /// The current set of actions in the background.
    #[allow(dead_code)]
    pub bg_action_pool: HashMap<ActionID, Action>,

    /// The logical, global time consumed by actions that have been committed already.
    pub committed_time: LogicalTime,

    /// INVARIANT: Thread IDs in `blocked` are absent from `run_queue`.
    pub blocked: BlockedPool,

    /// Kernel-blocked vfork parents and their children, once registered.
    vfork_barriers: BTreeMap<DetTid, Option<DetTid>>,

    /// Threads whose run-queue admission was recorded by a global-request
    /// handler while a `tentative_pop` transaction was live, deferred to the
    /// next deterministic drain point (`step2`) so it cannot mutate the run
    /// queue underneath the daemon's tentative selection. Keyed by `DetTid`
    /// so the drain order among co-pending admissions is canonical rather than
    /// lock-acquisition dependent. The stored value is the *unresolved*
    /// [`AdmitIntent`], so any side-selecting PRNG draw is deferred to the
    /// `DetTid`-ordered drain rather than performed in host RPC order. See
    /// [`Scheduler::admit_to_run_queue`] and
    /// [`Scheduler::drain_pending_run_queue_admissions`].
    pending_run_queue_admissions: BTreeMap<DetTid, AdmitIntent>,

    /// Run-queue *removals* requested by a global-request handler
    /// (`reconnect_after_exec` -> `logically_kill_thread`), deferred to the same
    /// deterministic drain point (`step2`). `RunQueue::remove_tid` carries the same
    /// `tentative_selection.is_none()` guard as the push operations, so a
    /// multi-threaded exec that reconnects on an asynchronous backend (DBI)
    /// inside the daemon's tentative window would otherwise trip it, poison the
    /// scheduler mutex, and hang the run.
    ///
    /// A `Retire` target is already logically dead (its `next_turns` entry is
    /// gone and its request is `ThreadExited`). `ReplaceThenAdmit` is the one
    /// Linux exception: a nonleader exec has installed a fresh registration at
    /// the destroyed leader's raw TID, but the old incarnation's physical queue
    /// slot must still be removed. Both are filtered from
    /// [`Scheduler::are_all_quiesced`] until this drain establishes the intended
    /// physical queue state.
    ///
    /// The map is drained before admissions. Ordinary retirement cancels a
    /// buffered admission for the same raw TID; the explicitly classified exec
    /// replacement preserves exactly one causally paired admission.
    pending_run_queue_removals: BTreeMap<DetTid, RemovalDisposition>,

    /// Child-TID futexes whose kernel clear may still be racing a guest join.
    cleared_child_tids: HashMap<FutexID, DetTid>,

    /// Whether exit-group teardown must explicitly cancel parked backend RPCs.
    cancel_killed_thread_rpcs: bool,

    /// Raw TIDs removed by logical teardown. Tombstones are permanent for the life of this
    /// scheduler: accepting Linux TID reuse would let delayed backend RPCs bind to a new thread.
    logically_killed_threads: BTreeSet<DetTid>,

    /// Accepted address-space incarnation for raw TIDs explicitly reused by exec.
    exec_incarnations: BTreeMap<DetTid, MmId>,

    /// Tombstoned SaBRe threads whose final asynchronous deregistration statistics were merged.
    /// Logical exit-group teardown and physical exit cleanup are distinct events.
    deregistration_accounted: BTreeSet<DetTid>,

    /// Whether the backend will report final physical process exits after logical cleanup.
    backend_reports_physical_process_exits: bool,

    /// SaBRe process leaders whose tool exit hook ran before the ptrace supervisor observed the
    /// final kernel exit status. While the run queue is empty, these prevent virtual timers from
    /// overtaking a child exit that is not physically waitable yet.
    pending_physical_process_exits: BTreeSet<DetPid>,

    /// Whether the backend defers spawning a vfork child until after the parent posts its
    /// continuation, so an unfulfilled vfork barrier at parent continuation means the child is
    /// still on its way rather than that the clone failed. See
    /// [`Config::backend_defers_vfork_child_registration`].
    backend_defers_vfork_child_registration: bool,

    /// Ac table of "locks held": which action is using which resources.
    /// A given resource can be held by at most one action at a given time.
    #[allow(dead_code)]
    pub resources: HashMap<ResourceID, ActionID>,

    /// Initially false, set to true when the first thread is running.
    /// Invariant: at the moment this becomes full, the queue is nonempty.
    pub started_up: Ivar<()>,

    /// A model of the the raw ancestry tree of threads, based on parentage at the point
    /// of thread creation.  This establishes a mapping from each thread to the child
    /// threads it has spawned.
    //
    // FUTURE OPTION:
    // If this is not used for purposes *other* than `exit_group` handling in the future,
    // we could probably rip it out and just refer to the `/proc/pid/task/` directory
    // to determine what threads exit upon `exit_group`.
    pub thread_tree: ThreadTree,

    /// Tracks the priorities of each thread. New threads should have an entry
    /// before being inserted into the runqueue.
    ///
    /// INVARIANT: Whenever the thread is normally in the run_queue, it's
    /// priority in the queue should match that stored here. "Abnormal"
    /// queueings include polling and eager IO polling.
    ///
    /// NB: BTreeMap over HashMap for deterministic printing.
    pub priorities: BTreeMap<DetTid, Priority>,

    /// Tracks explicit optional timeslices to run for each thread.
    /// If a guest is to be unblocked on a thread the guest will receive this
    /// information and needs to "cooperate" and setup it's preemption for the amount
    pub timeslices: BTreeMap<DetTid, Option<LogicalTime>>,

    /// Per-thread distribution of completed timeslice durations (virtual ns),
    /// collected from each thread as it deregisters at exit. Aggregated into the
    /// final run report. BTreeMap for deterministic iteration order.
    pub per_thread_timeslice: BTreeMap<DetTid, TimesliceStats>,

    /// A record of which preemptions occured on each thread.  Only used IF `--record-preemptions`
    /// was specified in the Config, otherwise this remains empty.
    pub preemption_writer: Option<PreemptionWriter>,

    /// An instance of replayer that is responsible for replaying events in case --replay-preemptions-from is specified
    pub replayer: Option<Replayer>,

    /// Count record_event calls which determines the event number if we're recording a schedule
    /// event trace.
    pub recorded_event_count: u64,

    /// A copy of the `Config::stacktrace_event` vector.  This is MUTABLE,
    /// because we pop events off as we handle them.  The u64 is an index into
    /// the (original) replay_cursor trace.
    pub stacktrace_events: Option<StacktraceEventsIter>,

    /// PRNG to drive any fuzzing of OS semantics (other than scheduling).
    fuzz_prng: Pcg64Mcg,

    /// Independent scheduler-seeded stream for post-fork ordering choices.
    post_fork_prng: Pcg64Mcg,

    /// A cached copy of the same (immutable) field in Config.
    stop_after_turn: Option<u64>,
    /// A cached copy of the same (immutable) field in Config.
    stop_after_iter: Option<u64>,
    /// A cached copy of the same (immutable) field in Config.
    recordreplay_modes: bool,
    /// A cached copy of the same (immutable) field in Config.
    fuzz_futexes: bool,
    /// A cached copy of the same (immutable) field in Config. When set (and only
    /// meaningful in chaos mode) the scheduler biases its nondeterminism points
    /// toward known race patterns rather than exploring uniformly.
    chaos_target_races: bool,

    /// Happens-before enforcement state, present only when the run carries a
    /// `HappensBeforeProgram`. Holds AFTER anchors until their BEFORE anchors
    /// fire, deterministically constructing an authored race ordering.
    happens_before: Option<HbRuntime>,
}

type StacktraceEventsIter = Peekable<IntoIter<(u64, Option<SchedEvent>, Option<PathBuf>)>>;

// type ThreadTree = HashMap<DetTid, Vec<DetTid>>;
#[derive(Debug, Clone, Default)]
pub struct ThreadTree {
    /// Invariant: this is None only if `tree` is also empty.
    /// That is any ThreadTree of size zero or more has a root.
    root: Option<DetTid>,
    /// Invariant: every `DetTid` in the tree has an entry here, though if it is a leaf,
    /// it will have an empty children-vector.
    tree: HashMap<DetTid, Vec<DetTid>>,

    /// The subset of threads that are also thread group leaders.  This tracks both the
    /// Tid, but it is (numerically) the same as Pid for group leaders in Linux.
    thread_group_leaders: HashSet<DetTid>,

    /// Go from a Tid to the Pid/Tid of the containing process (i.e. a reverse view of a
    /// transitive closure of `thread_tree`).  Every thread should have an entry in
    /// here. If, however, a thread is a group leader, this will map back to itself.
    thread_to_leader: HashMap<DetTid, DetPid>,

    /// Reverse map from a process (group-leader `DetPid`) to the `DetPid` of the
    /// process that created it. Populated when a new group leader is registered
    /// (a `clone`/`fork` without `CLONE_THREAD`); the root process has no entry.
    /// Used to target a deterministic child-exit `SIGCHLD` at the reaping
    /// parent. Entries are not removed on exit (mirroring `tree`/`thread_to_leader`);
    /// a stale parent is handled gracefully by `select_signal_target`.
    process_parent: HashMap<DetPid, DetPid>,
}

use pretty::Doc;
use pretty::RcDoc;

use self::replayer::DesyncStats;
use self::replayer::Replayer;

impl ThreadTree {
    /// Internal helper. Add a [child] process to the tree, with the parent being `None`
    /// if it's the root of the tree.
    fn add_edge(&mut self, parent: Option<DetTid>, child: DetTid) {
        match parent {
            None => {
                self.root = Some(child);
                // Ensure an entry, even if the children vector is empty:
                let _vec = self.tree.entry(child).or_default();
            }
            Some(p) => {
                let vec = self.tree.entry(p).or_default();
                vec.push(child);
                let _vec = self.tree.entry(child).or_default();
            }
        }
    }

    /// Read the children of a thread, which is assumed to have an entry in the tree.
    pub fn get_children(&mut self, parent: &DetTid) -> &Vec<DetTid> {
        self.tree
            .get(parent)
            .expect("Internal failure: tid was not found in ThreadTree")
    }

    /// Convert to pretty-printed document.
    ///
    /// For example, a binary tree of depth two may print as `(1 (2 3 4) (5 6 7))`,
    /// showing each thread ID grouped with its children.
    ///
    /// The thread_group_leaders argument is used for additional context into account when
    /// pretty-printing a `ThreadTree`.  This will indicate which children are within new
    /// thread groups using square brackets:
    ///
    ///   `[1 [2 [3] 4] (5 6 7)]`
    // TODO: it would also be nice to store a fixed prefix of the binary name and listing
    // that along with the thread ID.
    pub fn pretty_print(&self) -> String {
        fn walk<'a>(
            tt: &'a HashMap<DetTid, Vec<DetTid>>,
            tgl: &HashSet<DetTid>,
            current: &DetTid,
        ) -> RcDoc<'a, ()> {
            if let Some(children) = tt.get(current) {
                if tgl.contains(current) {
                    RcDoc::text("[")
                        .append(RcDoc::as_string(current))
                        .append(if children.is_empty() {
                            RcDoc::text("")
                        } else {
                            RcDoc::text(" ").append(
                                RcDoc::intersperse(
                                    children.iter().map(|x| walk(tt, tgl, x)),
                                    Doc::line(),
                                )
                                .nest(1)
                                .group(),
                            )
                        })
                        .append(RcDoc::text("]"))
                } else if children.is_empty() {
                    RcDoc::as_string(current)
                } else {
                    RcDoc::text("(")
                        .append(RcDoc::as_string(current))
                        .append(RcDoc::text(" "))
                        .append(
                            RcDoc::intersperse(
                                children.iter().map(|x| walk(tt, tgl, x)),
                                Doc::line(),
                            )
                            .nest(1)
                            .group(),
                        )
                        .append(RcDoc::text(")"))
                }
            } else {
                // This should be unreachable if the invariants are maintained:
                RcDoc::text("<ThreadTree corrupt, missing tid: ")
                    .append(RcDoc::as_string(current))
                    .append(RcDoc::text(">"))
            }
        }

        let root = match self.root {
            None => return "[]".into(),
            Some(root) => root,
        };

        let doc = walk(&self.tree, &self.thread_group_leaders, &root);
        let width = 100;
        let mut vec = Vec::new();
        doc.render(width, &mut vec).unwrap();
        String::from_utf8(vec).unwrap()
    }

    #[allow(dead_code)]
    /// Number of threads with entries in the tree.
    pub fn size(&self) -> usize {
        self.tree.len()
    }

    /// Simultaneously update the thread tree and leader tracking to reflect the creation
    /// of a new child thread.
    pub fn add_child(
        &mut self,
        parent_dettid: DetTid,
        child_dettid: DetTid,
        is_group_leader: bool,
    ) {
        // TODO(T78538674): virtualize pid/tid:
        if parent_dettid == child_dettid {
            self.add_edge(None, child_dettid);
        } else {
            self.add_edge(Some(parent_dettid), child_dettid);
        }
        if is_group_leader {
            self.thread_group_leaders.insert(child_dettid);
            self.thread_to_leader.insert(child_dettid, child_dettid);
            // Record the creating process (the parent thread's group leader) so a
            // deterministic child-exit SIGCHLD can later be targeted at it. The
            // root process (parent == child) has no parent process.
            if parent_dettid != child_dettid
                && let Some(parent_leader) = self.thread_to_leader.get(&parent_dettid).copied()
            {
                self.process_parent.insert(child_dettid, parent_leader);
            }
        } else {
            let parent_leader: DetPid =
                    *self
                        .thread_to_leader
                        .get(&parent_dettid)
                        .unwrap_or_else(|| {
                            panic!("recv_create_child_thread: parent {} of child dtid {} does not exist in thread_to_leader map!",
                                   parent_dettid, child_dettid);
                        });
            self.thread_to_leader.insert(child_dettid, parent_leader);
        }
    }

    /// The process that created `pid` (its parent process), if `pid` is not the
    /// root process. Returns a possibly-stale parent if that process has since
    /// exited; callers deliver through `select_signal_target`, which drops a
    /// signal to a `Gone` target.
    pub fn parent_process(&self, pid: &DetPid) -> Option<DetPid> {
        self.process_parent.get(pid).copied()
    }

    /// Return the set of thread IDs in the "same process" as me (same TGID), including
    /// myself.
    ///
    /// Locks: takes scheduler lock.
    pub fn my_thread_group(&mut self, me: &DetTid) -> Vec<DetTid> {
        let root_tid: DetTid = if self.thread_group_leaders.contains(me) {
            *me
        } else {
            *self
                .thread_to_leader
                .get(me)
                .expect("thread must be in to_leader table")
        };
        let mut stack: Vec<DetTid> = vec![root_tid];
        let mut acc: Vec<DetTid> = vec![];

        while let Some(first) = stack.pop() {
            if self.thread_group_leaders.contains(&first) && first != root_tid {
                continue; // Stop traversal when we walk into child processes.
            } else {
                acc.push(first);
            }
            let children = self.get_children(&first);
            stack.extend_from_slice(children);
        }
        assert!(acc.contains(me));
        acc
    }
}

impl std::fmt::Display for ThreadTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Print with empty thread_group_leaders since we don't have that information in
        // this context:
        write!(f, "{}", self.pretty_print())
    }
}

// TODO (T137183027, T137184765)
/// A simple backoff strategy while we have any realtime/polling elements in the system.
/// When all external polling is removed, we can remove this.
struct Backoff {
    count: u64,
}

impl Backoff {
    fn new() -> Self {
        Backoff { count: 0 }
    }

    async fn further(&mut self, blocking: bool) {
        self.count += 1;
        const YIELDS_FIRST: u64 = 10;
        if blocking {
            if self.count <= YIELDS_FIRST {
                std::thread::yield_now();
            } else {
                let round = self.count - YIELDS_FIRST;
                let micros = if round > 13 { 10_000 } else { 2 ^ round };
                std::thread::sleep(Duration::from_micros(micros));
            }
        } else if self.count <= YIELDS_FIRST {
            tokio::task::yield_now().await;
        } else {
            let round = self.count - YIELDS_FIRST;
            let micros = if round > 13 { 10_000 } else { 2 ^ round };
            tokio::time::sleep(Duration::from_micros(micros)).await;
        }
    }

    fn reset(&mut self) {
        self.count = 0;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) type SchedulerObserver = Arc<dyn Fn(&'static str) + Send + Sync>;

pub(crate) async fn sched_loop(sched: Arc<Mutex<Scheduler>>, timer: Arc<Mutex<GlobalTime>>) {
    sched_loop_inner(sched, timer, false, None).await;
}

pub(crate) async fn sched_loop_external(
    sched: Arc<Mutex<Scheduler>>,
    timer: Arc<Mutex<GlobalTime>>,
    observer: SchedulerObserver,
) {
    sched_loop_inner(sched, timer, true, Some(observer)).await;
}

async fn sched_loop_inner(
    sched: Arc<Mutex<Scheduler>>,
    timer: Arc<Mutex<GlobalTime>>,
    blocking_backoff: bool,
    observer: Option<SchedulerObserver>,
) {
    info!("[scheduler] daemon task starting up, waiting for guest thread start..");
    if let Some(observer) = &observer {
        observer("daemon task starting; waiting for guest thread");
    }
    let (iv, stop_after_iter) = {
        // Block until queue is populated.
        let sched = sched.lock().unwrap();
        (sched.started_up.clone(), sched.stop_after_iter)
    };
    iv.get().await;
    info!("[scheduler] guest in queue, scheduler proceeding..",);
    if let Some(observer) = &observer {
        observer("guest registered; deterministic scheduler proceeding");
    }
    let mut iter: u64 = 0;
    // We keep track of whether the last turn was a SKIP:
    let mut last_res = Err(SkipTurn);
    let mut backoff = Backoff::new();
    let mut observed_turn = false;

    loop {
        // TODO (T137183027, T137184765): as part of the current strategy for blocking IO ops (see
        // SPINNING below), we need to make sure that other threads can progress so we don't
        // busy-wait too tightly.
        if last_res.is_err() {
            backoff.further(blocking_backoff).await;
        } else {
            backoff.reset();
        }

        trace!("[scheduler] loop iteration {}", iter);
        if stop_after_iter.is_some() && iter > stop_after_iter.unwrap() {
            let sched = sched.lock().unwrap();
            tracing::warn!(
                "[scheduler] Early exit during sched loop iteration {} due to --stop-after-iter.  Summary:\n\n{}",
                iter,
                sched.full_summary()
            );
            immediate_fatal_exit(); // We don't want a backtrace of this thread.
        }
        iter += 1;

        // If there are NO threads left in the system, then we're truly done:
        {
            let sched = sched.lock().unwrap();
            if sched.run_queue.is_empty()
                && sched.blocked.is_empty()
                && sched.pending_physical_process_exits.is_empty()
                && sched.pending_run_queue_admissions.is_empty()
                && sched.pending_run_queue_removals.is_empty()
            {
                info!("[scheduler] run queue empty, exiting sched_loop.");
                if let Some(observer) = &observer {
                    observer("run queue empty; scheduler completed");
                }
                return;
            } else if let Some(stop) = sched.stop_after_turn
                && sched.turn > stop
            {
                tracing::warn!(
                    "[scheduler] Early exit during turn {} due to --stop-after-turn.  Summary:\n\n{}",
                    sched.turn,
                    sched.full_summary()
                );
                immediate_fatal_exit(); // We don't want a backtrace of this thread.
            }
        }

        // Otherwise we trust the turn function to either choose a runnable thread or wait
        // until something blocked is ready to run again.
        last_res = do_a_turn_blocking(sched.clone(), timer.clone(), &last_res).await;
        if last_res.is_ok() && !observed_turn {
            if let Some(observer) = &observer {
                observer("completed a deterministic scheduling turn");
            }
            observed_turn = true;
        }
    }
}

/// Not an error, but simply a turn that cannot do productive work.
#[derive(Debug, Clone)]
pub struct SkipTurn;

/// Advance turn by 1 turn, blocking when necessary to make it happen.
/// Return the outcome of the turn as well as which resources were used, if any.
///
/// WARNING: this is duplicated with the non-blocking `step` function below.
/// TODO: this duplication is temporary and they should be either combined or one removed soon.
pub async fn do_a_turn_blocking(
    sched: Arc<Mutex<Scheduler>>,
    global_time: Arc<Mutex<GlobalTime>>,
    last_turn: &Result<Resources, SkipTurn>,
) -> Result<Resources, SkipTurn> {
    // Loop until all threads are parked, then proceed:
    //
    // TODO: First, this check can move after step2, before we commit the action. Second,
    // it can later grow more sophisticated to only check the completion of dependent
    // actions, not all outsanding guest actions.
    loop {
        // We must read the queue carefully, because it can grow in the background
        // everytime we await.  However, while it can *grow*, it cannot change order, as
        // only the scheduler thread (us) actually rotates entries from the front to the back.
        let req_ivar = {
            let mut mg = sched.lock().unwrap();
            let arc = global_time.clone();

            let next_outstanding = mg.step1_check_quiescence(&arc, last_turn);
            match next_outstanding {
                None => {
                    trace!("Scheduler observed full quiescense, proceeding...");
                    break;
                }
                Some(iv) => iv.clone(),
            }
        };
        trace!("Scheduler wait for full quiescense, on {}...", req_ivar);
        let _ = req_ivar.await;
    }

    // Here we copy some information while holding the sched lock, and then release it so
    // we can `.await` below:
    let (next_dtid, req, resp) = {
        let mut sched = sched.lock().unwrap();
        sched.step2_process_blocked(&global_time)?;
        sched.step3_peek().ok_or(SkipTurn)?
    };

    // Step 1B: wait for the selected thread to make its request.
    trace!(
        "[sched-daemon] waiting for next thread (dtid {}) to park...",
        next_dtid
    );
    let rsrcs: Resources = match req.get().await {
        Err(ThreadExited) => {
            debug!(
                "[sched-daemon] woke up on request {}, but fizzling because next thread, {}, exited.",
                &req, &next_dtid
            );
            // The selected thread died while we awaited its request, so this turn
            // is skipped without reaching step4-6. `step3_peek` opened a
            // tentative_pop for `next_dtid`; close it here (undo, since the turn
            // did not commit) before returning. Otherwise the tentative selection
            // outlives the turn, and the next pass's step2 removal drain calls
            // `remove_tid` while `tentative_selection` is still `Some`, tripping
            // the run queue's transaction guard -- the "reconnect panic moved one
            // pass" defect. The dead thread's buffered removal drains
            // deterministically on the next pass, once this undo has closed the
            // window.
            sched.lock().unwrap().run_queue.undo_tentative_pop();
            return Err(SkipTurn);
        }
        Ok(r) => r,
    };
    trace!("[sched-daemon] daemon woke up on {}...", &req);

    // Since the scheduler is asynchronous, we need to check our assumptions.  Polling is
    // sufficient here because the thread cannot be racing with us to exit since we know
    // it is *already* parked.
    let mut mg = sched.lock().unwrap();
    mg.abort_turn_if_thread_vanished(next_dtid)?;

    // The logical COMMIT point for the turn is during step4:
    mg.step4_resource_block(next_dtid, &rsrcs, &resp)?;
    mg.step5_guest_unblock(next_dtid, &rsrcs, &resp)?;
    let sched_yield = rsrcs.resources.contains_key(&ResourceID::SchedYield);
    mg.step6_reenquue(next_dtid, sched_yield);
    if let Some(call) = rsrcs.as_exit_syscall() {
        mg.step7_simulate_exit_posthook(next_dtid, call, &global_time);
    }
    Ok(rsrcs)
}

// A futex request contains only one resource request, for FutexWait.
fn assert_futex_request(nextturn: &ThreadNextTurn) {
    match nextturn.req.try_read() {
        Some(Ok(req)) => {
            if !(req.resources.contains_key(&ResourceID::FutexWait) && req.resources.len() == 1) {
                panic!(
                    "assert_empty_request({}): internal invariant broken, expected empty resource request, found: {:?}",
                    nextturn.dettid, req
                )
            }
        }
        _ => panic!(
            "assert_empty_request({}): internal invariant broken, expected request for zero resources, instead found no request.",
            nextturn.dettid
        ),
    }
}

// Test if the request was from a futex_wait call.
fn is_futex_request(nextturn: &ThreadNextTurn) -> bool {
    match nextturn.req.try_read() {
        Some(Ok(req)) => Scheduler::is_x_turn(&req, &ResourceID::FutexWait),
        _ => false,
    }
}

/// Until panics are escalated properly, this encapsulates a way to exit the hermit container
/// entirely.
pub fn immediate_fatal_exit() {
    std::process::exit(1);
}

/// The result of consuming a SchedEvent during --replay-preemptions-from.  This represents some
/// decisions about what to do next, but are actions which we cannot implement inside
/// `consume_schedevent`.
pub struct ConsumeResult {
    /// Should we keep runnning this thread, if false we background the current thread after this schedevent to let the next thread run.
    pub keep_running: bool,
    /// A remaining (delta) timeslice this thread is required to run according to the replay schedule
    pub timeslice_remaining: Option<LogicalTime>,
    /// Should we print the stacktrace in the guest, as per --stacktrace-event
    pub print_stack: MaybePrintStack,
    /// The number of this event in the global total order of events.
    #[allow(dead_code)]
    pub event_ix: u64,
}

/// Any non-None response means that the guest should print its stack trace before proceeding, and
/// a response that further includes a path means print to a file at that location.
pub type MaybePrintStack = Option<Option<PathBuf>>;

enum ThreadStatus {
    // Not present in scheduler structures.
    Gone,
    Running,
    // Absent from run queue, but present in one of the blocked structures.
    NotRunning,
}

impl Scheduler {
    /// Create a new scheduler based on the configuration.
    pub fn new(cfg: &Config) -> Self {
        let (replayer, m_vec) = match &cfg.replay_schedule_from {
            Some(path) => {
                trace!("Scheduler loading trace from path {}", path.display());
                let vec = read_trace(path);
                trace!("Trace loaded, length {}", vec.len());

                let toprint = cfg
                    .stacktrace_event
                    .iter()
                    .map(|(ix, path)| (*ix, Some(vec[*ix as usize].clone()), path.clone()))
                    .collect();
                let mut replayer = Replayer::new(vec);
                replayer.replay_exhausted_panic = cfg.replay_exhausted_panic;
                replayer.die_on_desync = cfg.die_on_desync;
                (Some(replayer), Some(toprint))
            }
            None => (
                None,
                if cfg.stacktrace_event.is_empty() {
                    None
                } else {
                    let vec: Vec<_> = cfg
                        .stacktrace_event
                        .iter()
                        .map(|(ix, path)| (*ix, None, path.clone()))
                        .collect();
                    Some(vec)
                },
            ),
        };

        let stacktrace_events: Option<StacktraceEventsIter> = m_vec.map(|mut v| {
            v.sort_by_key(|(ix, _, _)| *ix);
            v.into_iter().peekable()
        });

        Self {
            preemption_writer: if cfg.record_preemptions {
                Some(PreemptionWriter::new(cfg.record_preemptions_to.clone()))
            } else {
                None
            },
            replayer,
            recorded_event_count: 0,
            stacktrace_events,
            stop_after_turn: cfg.stop_after_turn,
            stop_after_iter: cfg.stop_after_iter,
            recordreplay_modes: cfg.recordreplay_modes,
            run_queue: RunQueue::new(
                cfg.sched_heuristic,
                cfg.sched_seed(),
                cfg.sched_sticky_random_param,
            ),
            turn: 0,
            next_turns: Default::default(),
            bg_action_pool: Default::default(),
            committed_time: Default::default(),
            blocked: Default::default(),
            vfork_barriers: Default::default(),
            pending_run_queue_admissions: Default::default(),
            pending_run_queue_removals: Default::default(),
            cleared_child_tids: Default::default(),
            cancel_killed_thread_rpcs: cfg.cancel_killed_thread_rpcs,
            logically_killed_threads: Default::default(),
            exec_incarnations: Default::default(),
            deregistration_accounted: Default::default(),
            backend_reports_physical_process_exits: cfg.backend_reports_physical_process_exits,
            pending_physical_process_exits: Default::default(),
            backend_defers_vfork_child_registration: cfg.backend_defers_vfork_child_registration,
            resources: Default::default(),
            started_up: Default::default(),
            thread_tree: Default::default(),
            priorities: Default::default(),
            timeslices: Default::default(),
            per_thread_timeslice: Default::default(),
            fuzz_futexes: cfg.fuzz_futexes,
            chaos_target_races: cfg.chaos_target_races,
            fuzz_prng: Pcg64Mcg::seed_from_u64(cfg.fuzz_seed()),
            post_fork_prng: Pcg64Mcg::seed_from_u64(cfg.sched_seed() ^ 0x706f_7374_666f_726b),
            happens_before: cfg.happens_before.clone().map(HbRuntime::new),
        }
    }

    /// Record a newly created thread for happens-before `spawn_ordinal`
    /// resolution. A no-op unless a happens-before program is active.
    pub fn hb_note_spawn(&mut self, dettid: DetTid) {
        if let Some(hb) = self.happens_before.as_mut() {
            hb.note_spawn(dettid);
        }
    }

    /// Handle a happens-before checkpoint issued by `dettid` after its `count`th
    /// intercepted syscall (see `Detcore::handle_syscall_event`).
    ///
    /// Grants passage (firing every anchor at `SyscallCount(count)` on this
    /// thread and re-admitting any parked threads whose gate may now be open)
    /// unless a reached anchor is the AFTER endpoint of a Hard edge whose BEFORE
    /// anchor has not fired, in which case the thread is parked out of the run
    /// queue until a later firing wakes it. Mirrors the `SleepUntil` park/skip
    /// protocol: the request/response ivars are left intact so the re-admitted
    /// thread re-evaluates this same checkpoint on its next turn.
    fn hb_checkpoint(&mut self, dettid: DetTid, count: u64) -> Result<(), SkipTurn> {
        let (reached, blocked) = {
            let hb = self
                .happens_before
                .as_ref()
                .expect("hb checkpoint issued without a happens-before program");
            let reached = hb.anchors_at_syscall(dettid, count);
            let blocked = reached.iter().any(|name| hb.anchor_blocked(name));
            (reached, blocked)
        };

        if reached.is_empty() {
            // No anchor addresses this (thread, count); nothing to gate or fire.
            return Ok(());
        }

        if blocked {
            info!(
                "[scheduler] >>>>>>>\n\n NONCOMMIT turn {}, SKIP dettid {} held at happens-before \
                 anchor(s) {:?} (syscall count {}) awaiting a BEFORE anchor",
                self.turn, dettid, reached, count
            );
            self.happens_before.as_mut().unwrap().parked.insert(dettid);
            return self.skip_turn_blocked(dettid);
        }

        // Grant passage: fire the reached anchors. Only wake parked threads when a
        // new anchor actually fired, so an idempotent re-grant causes no churn.
        let mut newly_fired = false;
        {
            let hb = self.happens_before.as_mut().unwrap();
            for name in &reached {
                if hb.fired.insert(name.clone()) {
                    newly_fired = true;
                }
            }
        }
        if newly_fired {
            debug!(
                "[happens-before] dettid {} fired anchor(s) {:?} at syscall count {}",
                dettid, reached, count
            );
            // Defer the actual re-admission: we are inside `block_for_one_resource`
            // with a `tentative_pop` selection live, and pushing to the run queue
            // now would trip the queue's transaction assertion. `step3` flushes.
            self.happens_before.as_mut().unwrap().wake_pending = true;
        }
        Ok(())
    }

    /// If a happens-before anchor fired since the last check, re-admit every
    /// parked thread to the run queue so it re-evaluates its gate on its next
    /// turn. Threads still blocked re-park; the request/response ivars are
    /// untouched, so no request needs re-filling. Deterministic: parked threads
    /// are iterated in `DetTid` order.
    ///
    /// Called from `step3_peek` *before* the turn's `tentative_pop`, the only
    /// safe point to push to the run queue: anchors fire deep inside
    /// `block_for_one_resource` while a selection transaction is live, so the
    /// actual re-admission must be deferred to here.
    fn hb_flush_wakes(&mut self) {
        match self.happens_before.as_mut() {
            Some(hb) if hb.wake_pending => hb.wake_pending = false,
            _ => return,
        }
        let parked: Vec<DetTid> = self
            .happens_before
            .as_ref()
            .map(|hb| hb.parked.iter().copied().collect())
            .unwrap_or_default();
        for dettid in parked {
            self.happens_before.as_mut().unwrap().parked.remove(&dettid);
            if !self.run_queue.contains_tid(dettid) {
                let pos = self.runqueue_push_back(dettid);
                trace!(
                    "[happens-before] re-admitting parked dettid {} at queue position {}",
                    dettid, pos
                );
            }
        }
    }

    /// Fill in a resource request, which is exactly what might make the next logical
    /// step become unblocked.
    pub fn request_put(
        &mut self,
        req: &Ivar<SchedRequest>,
        rs: Resources,
        _global_time: &Arc<Mutex<GlobalTime>>,
    ) {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1041): A guest resource-request RPC can race an
        // asynchronous signal delivery. `force_unblock_thread` replaces
        // `next_turns[tid].req` with an already-full Ivar carrying an
        // `InboundSignal` request (see the `Ivar::full` at the bottom of this
        // file). If the guest's own resource-request then lands on that same
        // turn, the previous unconditional `req.put()` panicked with "Ivar
        // multiple put" (observed intermittently on `timeout 5 echo hi` under
        // `--strict --verify`, where a 5s SIGALRM races the child's `wait4`).
        // Tolerate exactly that case: drop the late guest request and let the
        // signal turn win — the interrupted syscall (e.g. `rt_sigsuspend`)
        // restarts and re-issues a fresh request on the next turn. Any other
        // double-put still panics, preserving the write-once Ivar invariant.
        // Mirrors the `try_put` guard in `logically_kill_thread` (PR-845).
        if let Some(dropped) = req.try_put(Ok(rs)) {
            let tolerated = matches!(
                req.try_read(),
                Some(Ok(existing))
                    if existing
                        .resources
                        .keys()
                        .any(|r| matches!(r, ResourceID::InboundSignal(_)))
            );
            if tolerated {
                trace!(
                    "[request_put] dropping late guest request {:?}; an async inbound signal already filled the request for this turn (req {})",
                    dropped, req
                );
            } else {
                panic!(
                    "Ivar multiple put exception in request_put! Attempted to write {:?} to {}; existing content is not an inbound-signal request.",
                    dropped, req
                );
            }
        }
    }

    /// Poll the resource request and *if* it is not currently observed to be full, return
    /// the IVar that *will* contain it in the future.
    fn check_request(&self, det_tid: &DetTid) -> Option<Ivar<SchedRequest>> {
        let nextturn = self.next_turns.get(det_tid).unwrap_or_else(|| {
            panic!(
                "[check_request] internal error: dettid {} queued but missing entry in next_turns",
                det_tid
            )
        });
        if nextturn.req.try_read().is_none() {
            Some(nextturn.req.clone())
        } else {
            None
        }
    }

    /// Returns None if all are parked, otherwise the unfilled request of the next we're waiting on.
    fn are_all_quiesced(&self) -> Option<Ivar<SchedRequest>> {
        // Skip raw TIDs whose old run-queue incarnation is pending removal.
        // `Retire` targets have no `next_turns` entry, while
        // `ReplaceThenAdmit` targets have a fresh registration that must not be
        // waited on until the drain removes the old physical slot and admits
        // that replacement.
        self.run_queue
            .tids()
            .filter(|dt| !self.pending_run_queue_removals.contains_key(dt))
            .find_map(|dt| self.check_request(dt))
    }

    /// Try to pop the next event from the sorted list of stacktrace_events, if it matches the given
    /// index.  This is idempotent, because subsequent attempts will just fizzle.
    fn try_pop_stacktrace_event(
        &mut self,
        current_ix: u64,
        observed: &SchedEvent,
    ) -> MaybePrintStack {
        let mut result = None;
        if let Some(iter) = &mut self.stacktrace_events
            && let Some((next_ix, event, m_path)) = iter.peek()
        {
            let go = if let Some(ev) = event {
                (*next_ix == current_ix && events_consistent(observed, ev))
                    || events_match(observed, ev)
            } else {
                *next_ix == current_ix
            };
            if go {
                info!(
                    "Now output stack trace for scheduled event #{} = {}:",
                    current_ix, observed,
                );
                if m_path.is_none() {
                    eprintln!(
                        "\nPrinting stack trace for scheduled event #{} = {}:",
                        current_ix, observed,
                    );
                }
                result = Some(m_path.clone());
                let _ = iter.next();
            }
        }
        result
    }

    /// Verify that the event we're replaying matches what just happened.  Set up the next
    /// (replayed) event to run.  Return true if the current thread will keep running and false if
    /// it needs to be descheduled.
    ///
    /// PreReq: we're running under --replay-schedule-from
    pub fn consume_schedevent(&mut self, observed: &SchedEvent) -> ConsumeResult {
        debug_assert!(self.replayer.is_some());
        let mytid = observed.dettid;

        if let Some((ix, action)) = self.replayer.as_mut().map(|r| {
            let current_ix = r.traced_event_count;
            (current_ix, r.observe_event(observed))
        }) {
            let print_stack = self.try_pop_stacktrace_event(ix, observed);
            debug!("Next ReplayAction = {:?}", action);

            match action {
                replayer::ReplayAction::Continue(timeslice_remaining) => {
                    return ConsumeResult {
                        keep_running: true,
                        print_stack,
                        event_ix: ix,
                        timeslice_remaining,
                    };
                }
                replayer::ReplayAction::Stop(StopReason::FatalDesync) => immediate_fatal_exit(),
                replayer::ReplayAction::Stop(StopReason::ReplayExausted) => immediate_fatal_exit(),
                replayer::ReplayAction::ContextSwitch(is_now, new_tid, timeslice_remaining) => {
                    self.requeue_with_new_priority(mytid, REPLAY_DEFERRED_PRIORITY);
                    self.requeue_with_new_priority(new_tid, REPLAY_FOREGROUND_PRIORITY);
                    if !self.run_queue.contains_tid(new_tid) {
                        // If it is not yet in next_turns, that is because it was JUST spawned and
                        // hasn't showed up yet, but it will by the next scheduler turn.
                        if self.next_turns.contains_key(&new_tid) {
                            tracing::warn!(
                                "Attempted to context switch to tid {}, but it is not runnable atm. This could be legitimate if it is awoken by another thread exiting (futex wake).",
                                new_tid
                            );
                            // TODO(T138906107): make this a fatal error when RESYNC capability is robust enough.
                            // immediate_fatal_exit();
                        }
                    }
                    self.timeslices.insert(new_tid, timeslice_remaining);
                    return ConsumeResult {
                        keep_running: !is_now,
                        print_stack,
                        event_ix: ix,
                        timeslice_remaining,
                    };
                }
            };
        }

        ConsumeResult {
            keep_running: true,
            print_stack: None,
            event_ix: 0,
            timeslice_remaining: None,
        }
    }

    /// Remove a thread from the deterministic scheduler.  In order to call this, the precondition
    /// is that this thread will execute no further (visible) instructions.
    ///
    /// This is called while the guest is running, not in the middle of a scheduler turn.
    ///
    /// This is IDEMPOTENT, and it may indeed be called twice, both to proactively remove a thread,
    /// and then reactively in response to an exit hook.
    pub fn logically_kill_thread(&mut self, dtid: &DetTid, detpid: &DetPid, mm: MmId) {
        if self.cancel_killed_thread_rpcs {
            self.logically_killed_threads.insert(*dtid);
        }
        // Remove from the runnable queue at the next deterministic drain. This
        // is safe even if an asynchronous exec reconnect races a live
        // tentative_pop: the handler never reaches the run queue's mutation
        // guard and cannot poison the scheduler mutex.
        self.deschedule_or_defer(*dtid);
        // Remove from all non-runnable pools:
        self.remove_blocking_entries(dtid);

        let _ = self.priorities.remove(dtid);
        match self.next_turns.remove(dtid) {
            None => {
                trace!(
                    "logically_kill_thread: thread already removed from scheduler: {}",
                    &dtid
                );
            }
            Some(nextturn) => {
                info!(
                    "logically_kill: Scheduler removing all knowledge of [det]tid {} in pid {}..",
                    dtid, detpid
                );
                // Put in a dummy request to unblock the scheduler that might be
                // waiting for the thread to park.
                //
                // WARNING: this try_put should potentially turn back into a put(), if we can narrow
                // down the exit scenarios and ensure that they happen when the guest is running and
                // has NOT filled its request to the scheduler yet.
                let request_was_pending = nextturn.req.try_put(Err(ThreadExited)).is_some();
                if request_was_pending && self.cancel_killed_thread_rpcs {
                    // AUTONOMOUS-BOT-IMPLEMENTED
                    // TODO-HUMAN-REVIEW(PR-845): Review killed-thread RPC cancellation.
                    nextturn.resp.try_put(SchedResponse::Signaled());
                }
                self.wake_futex_child_cleartid(
                    FutexID::private(mm, nextturn.child_tid_addr),
                    *dtid,
                );
            }
        }

        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#663)
        let live_process_thread = self
            .thread_tree
            .my_thread_group(detpid)
            .into_iter()
            .any(|tid| self.next_turns.contains_key(&tid));
        if !live_process_thread {
            let _ = self.begin_physical_process_exit(*detpid);
            self.blocked.timed_waiters.remove_process_timers(*detpid);
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1173): Review SaBRe exec incarnation reconciliation.
    /// Apply Linux's successful-exec rule when a backend reloads its tool.
    ///
    /// Every sibling disappears. If a non-leader called exec, Linux also changes
    /// that surviving task's TID to the process leader's TID. In that case the old
    /// caller registration is retired and a fresh leader registration is installed
    /// before it is removed, so process-exit barriers cannot observe a transiently
    /// empty thread group.
    pub fn reconnect_after_exec(&mut self, reconnect: ExecReconnect) -> Vec<DetTid> {
        let ExecReconnect {
            caller,
            new_leader,
            detpid,
            pre_exec_mm,
            post_exec_mm,
            child_tid_addr,
            reconnect_priority,
        } = reconnect;
        let group = self.thread_tree.my_thread_group(&detpid);
        assert!(group.contains(&caller));
        assert!(group.contains(&new_leader));
        self.exec_incarnations.insert(new_leader, post_exec_mm);

        if caller == new_leader {
            let siblings: Vec<_> = group.into_iter().filter(|tid| *tid != caller).collect();
            for sibling in &siblings {
                self.logically_kill_thread(sibling, &detpid, pre_exec_mm);
                self.timeslices.remove(sibling);
            }
            self.remove_exec_vfork_barriers(&siblings);
            return siblings;
        }

        let survivor_priority = self
            .priorities
            .get(&caller)
            .copied()
            .or(reconnect_priority)
            .expect("exec caller must have a scheduler priority");
        let mut retired = Vec::new();
        for old_tid in group.into_iter().filter(|tid| *tid != caller) {
            self.logically_kill_thread(&old_tid, &detpid, pre_exec_mm);
            self.timeslices.remove(&old_tid);
            retired.push(old_tid);
        }

        // The leader identity was occupied by a thread the kernel destroyed as
        // part of this exec. This is the one intentional exception to permanent
        // raw-TID tombstones: the pending exec record proves why Linux reused it.
        self.logically_killed_threads.remove(&new_leader);
        self.deregistration_accounted.remove(&new_leader);
        self.pending_physical_process_exits.remove(&detpid);
        assert!(
            self.next_turns
                .insert(
                    new_leader,
                    ThreadNextTurn {
                        dettid: new_leader,
                        child_tid_addr,
                        req: Ivar::new(),
                        resp: Ivar::new(),
                    },
                )
                .is_none(),
            "retired exec leader still had a scheduler registration"
        );
        self.priorities.insert(new_leader, survivor_priority);
        if let Some(writer) = &mut self.preemption_writer {
            writer.set_current(new_leader, survivor_priority);
        }
        // Post-exec reconnection can arrive asynchronously on backends whose
        // exec-child self-bootstraps outside a scheduler turn (DBI), so route
        // the new leader's admission (a run-queue *push*) through the
        // tentative-safe buffer rather than pushing directly.
        //
        // The exec caller's fresh next-turn request is the causal anchor. Step5
        // installed that empty Ivar before the caller began executing exec, so
        // the daemon cannot pass step1 while the successful exec is in flight.
        // This handler records the complete old-leader removal/replacement
        // admission pair before `logically_kill_thread(caller)` resolves that
        // request. The scheduler mutex then prevents step2 from observing only
        // half of the handoff. Host delay can move the handler relative to the
        // daemon's wait, but cannot change first-drain membership.
        //
        // The *removals* in this handler (`logically_kill_thread` ->
        // `run_queue.remove_tid`, for the exec caller and its siblings, above
        // and below) are likewise tentative-safe: `logically_kill_thread` now
        // routes the run-queue removal through `deschedule_or_defer`, which
        // buffers it to the same deterministic `step2` drain. The old leader's
        // removal is explicitly classified as `ReplaceThenAdmit`, so the drain
        // removes its physical queue slot without cancelling the new
        // incarnation. `are_all_quiesced` filters every pending removal key;
        // ordinary targets are logically dead, while the replacement key is not
        // runnable until that old slot has been removed and its admission
        // applied. No handler mutates the queue inside a tentative window.
        self.replace_retired_run_queue_incarnation(new_leader, AdmitIntent::Fixed(AdmitSide::Back));
        self.started_up.try_put(());

        self.logically_kill_thread(&caller, &detpid, pre_exec_mm);
        self.timeslices.remove(&caller);
        retired.push(caller);
        self.remove_exec_vfork_barriers(&retired);
        retired
    }

    fn remove_exec_vfork_barriers(&mut self, retired: &[DetTid]) {
        self.vfork_barriers.retain(|parent, child| {
            !retired.contains(parent) && !child.is_some_and(|tid| retired.contains(&tid))
        });
    }

    #[cfg(test)]
    pub(crate) fn vfork_barrier_mentions(&self, dettid: DetTid) -> bool {
        self.vfork_barriers
            .iter()
            .any(|(parent, child)| *parent == dettid || child == &Some(dettid))
    }

    #[cfg(test)]
    pub(crate) fn install_test_vfork_barrier(&mut self, parent: DetTid, child: DetTid) {
        self.vfork_barriers.insert(parent, Some(child));
    }

    #[cfg(test)]
    pub(crate) fn install_test_exec_incarnation(&mut self, dettid: DetTid, mm: MmId) {
        self.exec_incarnations.insert(dettid, mm);
    }

    // TODO-HUMAN-REVIEW(PR-1023): Review fail-closed SaBRe thread tombstones.
    pub(crate) fn thread_is_logically_killed(&self, dettid: DetTid) -> bool {
        self.cancel_killed_thread_rpcs && self.logically_killed_threads.contains(&dettid)
    }

    pub(crate) fn rpc_incarnation_matches(&self, dettid: DetTid, mm: MmId) -> bool {
        self.exec_incarnations
            .get(&dettid)
            .is_none_or(|expected| *expected == mm)
    }

    /// Mark a physical exit cleanup as accounted. Non-cancelling backends preserve their existing
    /// behavior; SaBRe teardown may deliver the cleanup after an earlier logical tombstone.
    pub(crate) fn note_deregistration_accounted(&mut self, dettid: DetTid) -> bool {
        !self.cancel_killed_thread_rpcs || self.deregistration_accounted.insert(dettid)
    }

    /// Install a barrier between SaBRe's logical process-leader exit hook and the final ptrace
    /// wait status. Other backends retain their existing lifecycle behavior.
    pub(crate) fn begin_physical_process_exit(&mut self, detpid: DetPid) -> bool {
        if self.backend_reports_physical_process_exits {
            let inserted = self.pending_physical_process_exits.insert(detpid);
            if inserted {
                trace!(
                    "[detcore, dpid {}] waiting for final physical process exit",
                    detpid
                );
            }
            inserted
        } else {
            false
        }
    }

    /// Release the exact process barrier when the ptrace supervisor receives its final `Exited`
    /// or `Signaled` wait status. At that lifecycle point the process is physically waitable.
    pub(crate) fn complete_physical_process_exit(&mut self, detpid: DetPid) -> bool {
        self.pending_physical_process_exits.remove(&detpid)
    }

    /// Release every physical-exit barrier after the backend supervisor has drained all tracees.
    pub(crate) fn release_all_physical_process_exits(&mut self) -> usize {
        let released = self.pending_physical_process_exits.len();
        self.pending_physical_process_exits.clear();
        released
    }

    /// Remove entries from everywhere that non-runnable threads lurk.
    fn remove_blocking_entries(&mut self, dtid: &DetTid) {
        self.blocked.timed_waiters.remove(*dtid);
        let _ = self.blocked.external_io_blockers.remove(dtid);
        self.blocked.timed_out_futex_waiters.remove(dtid);
        self.blocked.sigchld_deferred.remove(dtid);
        self.blocked.sigchld_ready.remove(dtid);
        self.pending_run_queue_admissions.remove(dtid);
        let _ = self.remove_futex_waiter(dtid);
    }

    fn remove_futex_waiter(&mut self, dettid: &DetTid) -> bool {
        let mut removed = 0;
        self.blocked.futex_waiters.retain(|_, waiters| {
            let before = waiters.len();
            waiters.retain(|waiter| &waiter.dettid != dettid);
            removed += before - waiters.len();
            !waiters.is_empty()
        });
        assert!(removed <= 1, "thread was registered on multiple futexes");
        removed == 1
    }

    /// Put a Futex waiter to sleep, to be awoken by `wake_futex_waiter`.
    pub fn sleep_futex_waiter(
        &mut self,
        dettid: &DetTid,
        futexid: FutexID,
        maybe_timeout: Option<LogicalTime>,
        bitset: u32,
    ) {
        let nxt = self
            .next_turns
            .get(dettid)
            .expect("Missing next_turns entry");
        let entry: &mut Vec<_> = self.blocked.futex_waiters.entry(futexid).or_default();
        entry.push(FutexWaiter {
            dettid: *dettid,
            response: nxt.resp.clone(),
            bitset,
        });
        // When we park, we use a resource request to signal WHAT we're blocking on.  But this is
        // not quite the same as when an active thread in the runqueue blocks on a resource, because
        // we're not actually waiting on the scheduler giving us the resource.  We're waiting in the
        // futex_waiters pool until a waker comes along.
        let mut rsrc = Resources::new(*dettid);
        rsrc.insert(ResourceID::FutexWait, Permission::R);
        nxt.req.put(Ok(rsrc));
        trace!(
            "[dtid {}] Waiter blocking on futex {:?}, now {} waiters, on {}",
            &dettid,
            &futexid,
            entry.len(),
            nxt.resp,
        );
        // A futex with timeout waits in both the futex_waiters and timed_events structures:
        if let Some(target_time) = maybe_timeout {
            self.blocked.timed_waiters.insert(target_time, *dettid);
        }
    }

    /// Reschedule a single thread that has been blocked on futex.
    pub fn wake_futex_waiter(&mut self, waiter: FutexWaiter) {
        let waiterid = waiter.dettid;
        let waiter_ivar = waiter.response;
        debug_assert!(!self.run_queue.contains_tid(waiterid));

        // If it was registered as a waiter-with-timeout, remove it:
        self.blocked.timed_waiters.remove(waiterid);

        // Put the woken thread back into circulation:
        let pos = self.runqueue_push_back(waiterid);
        trace!(
            "[detcore] Woke one thread, dtid: {}, ivar {}, scheduled at position {}",
            &waiterid, &waiter_ivar, pos,
        );
        let nxt = self
            .next_turns
            .get_mut(&waiterid)
            .expect("Thread must have an entry in next_turns");
        assert_futex_request(nxt);
        // N.B. We don't write the response here.  That's for the scheduler to do.
        // But with a place in the queue, and a request filled, this thread
        // is ready to run in normal order.
    }

    fn choose_futex_wakees(
        &mut self,
        vec: &mut Vec<FutexWaiter>,
        num_woken: usize,
    ) -> Vec<FutexWaiter> {
        if self.fuzz_futexes {
            let rng = &mut self.fuzz_prng;
            debug!(
                "[fuzz-futexes] selecting {} tids, pre shuffle: {:?}",
                num_woken,
                vec.iter().map(|x| x.dettid).collect::<Vec<DetTid>>()
            );

            // No need to actually use the results here since vec was mutated:
            let (_extracted, _remain) = &vec[..].partial_shuffle(rng, num_woken);

            info!(
                "[fuzz-futexes] selecting {} tids, post shuffle: {:?}",
                num_woken,
                vec.iter().map(|x| x.dettid).collect::<Vec<DetTid>>()
            );
        }
        // just take the first N, in whatever deterministic order they are in:
        vec.split_off(vec.len() - num_woken)
    }

    /// Reschedule all threads blocked on a particular futex.
    pub fn wake_futex_waiters(
        &mut self,
        _waker_dettid: DetTid,
        futexid: FutexID,
        max_to_wake: i32,
        wake_mask: u32,
    ) -> u64 {
        if max_to_wake == 0 {
            trace!("[detcore] Futex wake of 0 waiters necessarily fizzles...");
            return 0;
        }
        let mut vec: Vec<FutexWaiter> = {
            match self.blocked.futex_waiters.get_mut(&futexid) {
                None => {
                    trace!(
                        "[detcore] Futex wake {} waiters FIZZLED -- none waiting",
                        max_to_wake
                    );
                    return 0;
                }
                Some(r) => std::mem::take(r),
            }
        };
        trace!(
            "Waking up to {} Futex waiters, out of {} waiting.",
            max_to_wake,
            vec.len(),
        );
        let mut matching = take_matching_futex_waiters(&mut vec, wake_mask);
        let num_woken: usize = std::cmp::min(matching.len(), max_to_wake.try_into().unwrap());
        let to_wake = self.choose_futex_wakees(&mut matching, num_woken);

        assert_eq!(to_wake.len(), num_woken);
        for waiter in to_wake {
            self.wake_futex_waiter(waiter);
        }
        vec.extend(matching);
        // Put back what wasn't woken up:
        if !vec.is_empty() {
            let junk = self.blocked.futex_waiters.insert(futexid, vec);
            assert!(junk.unwrap().is_empty());
        }
        num_woken as u64
    }

    /// Simulate the effect of CLONE_CHILD_CLEARTID.
    pub fn wake_futex_child_cleartid(&mut self, futid: FutexID, dettid: DetTid) {
        self.cleared_child_tids.insert(futid, dettid);
        debug!(
            "simulate CLONE_CHILD_CLEARTID on futex {:?}, wake one",
            futid
        );
        // Wakes only one thread, as per:
        // https://man7.org/linux/man-pages/man2/set_tid_address.2.html
        self.wake_futex_waiters(dettid, futid, 1, u32::MAX);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-845): Review late CLONE_CHILD_CLEARTID wait recovery.
    /// Whether a futex word still names the child that was logically cleared.
    pub(crate) fn child_tid_was_cleared(&self, futid: FutexID, observed: i32) -> bool {
        self.cleared_child_tids
            .get(&futid)
            .is_some_and(|dettid| dettid.as_raw() == observed)
    }

    /// Step: Before we select which thread to run, first we check if some internal data
    /// structure maintenance is necessary, i.e. moving timed events from the waiting pool
    /// to the run queue. It manipulates scheduler data structures accordingly.
    fn step2_process_blocked(
        &mut self,
        global_time: &Arc<Mutex<GlobalTime>>,
    ) -> Result<(), SkipTurn> {
        // Apply run-queue mutations deferred by asynchronous global-request
        // handlers first, at this fixed deterministic point, before any early
        // return below and before step3 opens a tentative-pop window. Removals
        // drain before admissions so a thread killed while an admission was
        // still buffered is not re-enqueued.
        self.drain_pending_run_queue_removals();
        self.drain_pending_run_queue_admissions();
        self.step2a_wait_for_vfork_barrier()?;
        self.step2b_process_timed(); // May populate run_queue.
        self.step2c_process_io_blockers()?;
        self.step2e_process_signal_deferred(); // May populate run_queue.
        self.step2d_handle_empty_queue(global_time)?;
        Ok(())
    }

    /// Re-admit parents whose host-async `SIGCHLD` was parked in
    /// `blocked.sigchld_deferred` (see `block_for_one_resource`). Uses the same
    /// deterministic-work-first gate as `step2c_process_io_blockers`: a deferred
    /// signal is delivered only once the run queue holds no ordinary (non-poller)
    /// guest work, so its commit order is fixed by the scheduler rather than by
    /// host signal-arrival timing. Runs after external-IO harvesting so a ready
    /// IO continuation is always ordered ahead of a deferred signal.
    fn step2e_process_signal_deferred(&mut self) {
        if self.blocked.sigchld_deferred.is_empty() {
            return;
        }
        let only_pollers = match self.run_queue.first_priority() {
            Some(fp) => fp >= LAST_PRIORITY,
            None => true,
        };
        if !self.run_queue.is_empty() && !only_pollers {
            return;
        }
        // BTreeSet drains in sorted DetTid order, giving a canonical admission
        // order when several parents are owed a signal at the same quiescence.
        let ready = std::mem::take(&mut self.blocked.sigchld_deferred);
        for dtid in ready {
            info!("[step2] Re-admit deferred SIGCHLD for dtid {:?}", dtid);
            self.blocked.sigchld_ready.insert(dtid);
            self.run_queue.push_eager_io_repoll(dtid);
        }
    }

    /// Keep scheduling inside an active vfork until the parent can continue.
    /// Before child registration no guest may run; afterward step 3 admits only
    /// the child. A failed clone reaches the parent continuation without a child.
    ///
    /// On the ptrace backend the kernel keeps the vfork parent blocked inside the injected
    /// `clone(2)` until the child execs or exits, so a registered child (barrier `Some`) is always
    /// present by the time the parent posts its continuation; an unfulfilled barrier (`None`) at
    /// that point therefore means the clone failed and the barrier must be dropped. On a backend
    /// that defers the child spawn (see `backend_defers_vfork_child_registration`, e.g. KVM) the
    /// child registers only *after* the parent posts its continuation, so an unfulfilled barrier at
    /// parent continuation means the child is still on its way and the barrier must be kept.
    fn step2a_wait_for_vfork_barrier(&mut self) -> Result<(), SkipTurn> {
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1152): Review deferred vfork child registration.
        let defers_registration = self.backend_defers_vfork_child_registration;
        let completed_parents: Vec<_> = self
            .vfork_barriers
            .iter()
            .filter_map(|(parent, registered_child)| {
                let child_registered = registered_child.is_some();
                let remove = match self
                    .next_turns
                    .get(parent)
                    .and_then(|turn| turn.req.try_read())
                {
                    // The parent exited: there will be no child; drop the barrier.
                    Some(Err(ThreadExited)) => true,
                    Some(Ok(resources)) => {
                        let vfork_failed = resources
                            .resources
                            .keys()
                            .any(|resource| matches!(resource, ResourceID::VforkFailed(_)));
                        let at_continue = resources.resources.keys().any(|resource| {
                            matches!(resource, ResourceID::BlockedExternalContinue(_))
                        });
                        // A failed injected clone is an explicit deterministic outcome: no child
                        // can ever register, so cancel the barrier on every backend. Successful
                        // deferred spawns retain the ordinary continuation and keep waiting.
                        // At parent continuation, drop a fulfilled barrier as normal cleanup. An
                        // unfulfilled barrier is a failed clone only when the backend kept the
                        // parent blocked until the child registered; when the backend defers child
                        // registration the child is still coming, so keep waiting.
                        vfork_failed || (at_continue && (child_registered || !defers_registration))
                    }
                    _ => false,
                };
                remove.then_some(*parent)
            })
            .collect();
        for parent in completed_parents {
            self.vfork_barriers.remove(&parent);
        }

        if self.vfork_barriers.values().all(Option::is_some) {
            Ok(())
        } else {
            trace!(
                "waiting for vfork child registration from parents {:?}",
                self.vfork_barriers
            );
            Err(SkipTurn)
        }
    }

    /// Check whether it is time for the *earliest* time-based event to execute INSTEAD of
    /// dispatching from the normal run queue.  Manipulates scheduler data structures
    /// accordingly.
    fn step2b_process_timed(&mut self) {
        if let Some((time_ns, evt)) = self
            .blocked
            .timed_waiters
            .pop_if_before(self.committed_time)
        {
            match evt {
                TimedEvent::ThreadEvt(dtid) => self.wake_timed_event(time_ns, dtid),
                TimedEvent::SignalEvt(
                    timed_waiters::SignalTimerId::ChildExit { parent, .. },
                    dtid,
                    sig,
                ) => {
                    // Deterministic child-exit SIGCHLD. If the host-async signal
                    // already arrived and was parked by the InboundSignal deferral
                    // gate, release it now at this logical time — that real signal
                    // is sufficient, so do not also synthesize one (avoids a
                    // duplicate delivery). Otherwise synthesize the delivery so the
                    // parent is notified deterministically regardless of host
                    // signal latency. Either way mark the parent `sigchld_ready` so
                    // its InboundSignal turn is granted here rather than deferred a
                    // second time. Releasing the deferred signal at a logical
                    // deadline (rather than only at run-queue quiescence, as
                    // step2e does) is what breaks the redis_deep starvation
                    // deadlock: a busy sibling can no longer starve the reaper.
                    self.blocked.sigchld_ready.insert(parent);
                    if self.blocked.sigchld_deferred.remove(&parent) {
                        self.run_queue.push_eager_io_repoll(parent);
                    } else {
                        self.fire_alarm(parent, dtid, sig);
                    }
                }
                TimedEvent::SignalEvt(id, dtid, sig) => self.fire_alarm(id.process(), dtid, sig),
            }
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    fn fire_alarm(&mut self, dpid: DetPid, dtid: DetTid, sig: Signal) {
        let Some(target) = self.select_signal_target(dpid, Some(dtid)) else {
            info!(
                "[dpid {}] Alarm expired after its target exited; ignoring.",
                dpid
            );
            return;
        };
        info!(
            "[dtid {}] Alarm fired, delivering signal {} to guest.",
            target, sig
        );
        self.signal_guest(target, sig);
    }

    // Follow Linux semantics for delivering a signal to a thread within a process group.
    // Optionally take a hint on which tid detcore would *like* to deliver to, if it is available.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    fn select_signal_target(&mut self, detpid: DetPid, m_dettid: Option<DetTid>) -> Option<DetTid> {
        if !self.thread_tree.thread_group_leaders.contains(&detpid) {
            return None;
        }

        // Targeted chaos (T137242449): a process-directed signal may legally be
        // handled by any thread in the group that does not block it. Instead of
        // always steering it to the hinted/leader thread, pick a random eligible
        // thread to surface signal-timing races. This stays reproducible under a
        // fixed `--fuzz-seed`.
        if self.chaos_target_races {
            let group = self.thread_tree.my_thread_group(&detpid);
            let eligible: Vec<DetTid> = group
                .into_iter()
                .filter(|t| !matches!(self.thread_status(*t), ThreadStatus::Gone))
                .collect();
            if let Some(chosen) = chaos_pick(&mut self.fuzz_prng, &eligible) {
                info!(
                    "[targeted-chaos] delivering process-directed signal to random group thread {} (of {:?})",
                    chosen, eligible
                );
                return Some(chosen);
            }
        }

        if let Some(dettid) = m_dettid {
            match self.thread_status(dettid) {
                ThreadStatus::Gone => {}
                ThreadStatus::Running | ThreadStatus::NotRunning => {
                    return Some(dettid);
                }
            }
        }
        match self.thread_status(detpid) {
            ThreadStatus::Gone => self.process_signal_targets(detpid).into_iter().next(),
            ThreadStatus::Running | ThreadStatus::NotRunning => Some(detpid),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    /// Return the scheduler's live threads for a positive process ID.
    pub fn process_signal_targets(&mut self, detpid: DetPid) -> Vec<DetTid> {
        if !self.thread_tree.thread_group_leaders.contains(&detpid) {
            return Vec::new();
        }
        let mut targets = self.thread_tree.my_thread_group(&detpid);
        targets.retain(|tid| self.next_turns.contains_key(tid));
        targets.sort();
        targets
    }

    fn wake_timed_event(&mut self, time_ns: LogicalTime, dettid: DetTid) {
        let futex_timed_out = {
            let next_turn = self
                .next_turns
                .get(&dettid)
                .expect("internal invariant broken");
            is_futex_request(next_turn)
        };
        if futex_timed_out {
            assert!(self.remove_futex_waiter(&dettid));
            assert!(self.blocked.timed_out_futex_waiters.insert(dettid));
        }

        if enabled!(Level::TRACE) {
            if futex_timed_out {
                info!(
                    "[sched-step2] Time-based event on thread {} (time {}, committed time {}) - futex wait timed out!",
                    dettid, time_ns, self.committed_time
                );
            } else {
                info!(
                    "[sched-step2] Time-based event on thread {} (time {}) jumping back to the head of it's priority at global(committed) time {}",
                    dettid, time_ns, self.committed_time
                );
            }
        }
        self.runqueue_push_front(dettid);
    }

    /// Send a signal to the guest. A scheduler-parked thread is made runnable immediately. SaBRe
    /// external syscalls remain blocked until the signal interrupts them and their real
    /// continuation RPC becomes visible; other backends retain their existing immediate requeue.
    fn signal_guest(&mut self, dettid: DetTid, signal: Signal) {
        debug!(
            "[dtid {}] deliver signal {} physically to guest thread.",
            dettid, signal
        );
        let has_external_blocker = self.blocked.external_io_blockers.contains_key(&dettid);
        let await_external_continuation =
            self.backend_reports_physical_process_exits && has_external_blocker;
        if cfg!(debug_assertions) && !await_external_continuation {
            let nxtturn = self
                .next_turns
                .get(&dettid)
                .expect("internal invariant broken");
            assert!(
                nxtturn.req.try_read().is_some(),
                "signal_guest: thread should be parked in the scheduler"
            );
        }
        let pid = Pid::from_raw(dettid.as_raw()); // TODO(T78538674): virtualize pid/tid:
        signal::kill(pid, signal).expect("signal::kill to go through");
        if await_external_continuation {
            return;
        }

        // Now that the thread is signaled, it needs to be runnable for the scheduler to continue it.
        match self.thread_status(dettid) {
            ThreadStatus::Gone => {
                panic!(
                    "signal_guest: should not have just delivered a signal to a nonexistent thread..."
                );
            }
            ThreadStatus::Running => {
                let is_internal_io_polling = self
                    .next_turns
                    .get(&dettid)
                    .and_then(|next_turn| next_turn.req.try_read())
                    .is_some_and(|request| {
                        request.is_ok_and(|resources| {
                            resources
                                .resources
                                .contains_key(&ResourceID::InternalIOPolling)
                        })
                    });

                if is_internal_io_polling {
                    assert!(self.run_queue.remove_tid(dettid));
                    let mut rsrcs = Resources::new(dettid);
                    rsrcs.insert(ResourceID::InboundSignal(SigWrapper(signal)), Permission::W);
                    self.force_unblock_thread(dettid, rsrcs);
                }
                // TODO(T137242449): other runnable requests could be reprioritized to run
                // sooner, but for now we leave their priorities alone.
            }
            ThreadStatus::NotRunning => {
                let mut rsrcs = Resources::new(dettid);
                rsrcs.insert(ResourceID::InboundSignal(SigWrapper(signal)), Permission::W);
                self.force_unblock_thread(dettid, rsrcs);
            }
        }
    }

    // Force a thread out blocking and into the runnable state, replacing its resource request.
    fn force_unblock_thread(&mut self, dettid: DetTid, rsrcs: Resources) {
        info!(
            "[dtid {}] removing blocking entries and requeuing thread",
            dettid
        );
        self.remove_blocking_entries(&dettid);

        if let Some(nxt) = self.next_turns.get_mut(&dettid) {
            // Counterfeit the entry as though the thread had requested this resource from the start:
            nxt.req = Ivar::full(Ok(rsrcs));
        }

        // Targeted chaos (T137242449): a force-unblocked thread (e.g. woken by a
        // signal or ready I/O) is normally requeued at the back of its priority
        // level, so it runs after everything already queued. Randomizing whether
        // it jumps to the front instead varies the order in which a just-woken
        // thread races the threads it was contending with -- surfacing
        // lock-ordering / wakeup-ordering races. Reproducible under `--fuzz-seed`.
        let to_front = self.chaos_target_races
            && chaos_pick(&mut self.fuzz_prng, &[true, false]).unwrap_or(false);
        if to_front {
            self.runqueue_push_front(dettid);
        } else {
            self.runqueue_push_back(dettid);
        }
    }

    /// Check on threads that were backgrounded performing external IO.
    fn step2c_process_io_blockers(&mut self) -> Result<(), SkipTurn> {
        if !self.blocked.external_io_blockers.is_empty() {
            // A nondeterministic snapshot of which blocking IO actions are ready right now:
            let ready: Vec<DetTid> = self
                .blocked
                .external_io_blockers
                .iter()
                .filter(|(dtid, op_id)| {
                    let nt = self
                        .next_turns
                        .get(dtid)
                        .expect("internal invariant broken");
                    if let Some(Ok(req)) = nt.req.try_read() {
                        assert_eq!(external_continue_id(&req), **op_id);
                        true
                    } else {
                        false
                    }
                })
                .map(|(dtid, _)| *dtid)
                .collect();
            debug!(
                "Nondeterministic status of blocking IO: out of {}, completed on {}, dtids: {:?}",
                self.blocked.external_io_blockers.len(),
                ready.len(),
                ready
            );

            // FIXME TODO (T137183027): for record/replay to work properly, we need to ALLOW the
            // "Nondeterminstic algorithm" below, but record & replay those scheduler events. In
            // the meantime, use a deterministic eager policy once there is no other runnable work.
            if self.recordreplay_modes {
                // Only *real* deterministic work should defer external-IO harvesting.
                // Internal pollers sit at LAST_PRIORITY and are frequently spinning on
                // the very result an external-IO blocker will produce (e.g. the reader
                // side of `echo hi | cat`, where one stage uses InternalIOPolling while
                // its peer is backgrounded on BlockingExternalIO). Treat a run queue
                // that holds only pollers as "no deterministic work" so we still harvest
                // completed IO below; a queued poller must not starve a ready blocker.
                let only_pollers = if let Some(fp) = self.run_queue.first_priority() {
                    fp >= LAST_PRIORITY
                } else {
                    true
                };

                // Deterministic work is runnable: let it proceed. Waiting here while such
                // work exists can deadlock thread creation -- the parent and new child
                // cannot complete clone while an existing worker blocks indefinitely in
                // epoll_wait.
                if !self.run_queue.is_empty() && !only_pollers {
                    return Ok(());
                }

                // Reschedule every blocker whose IO has completed so its continuation can
                // consume the recorded result. Harvesting all ready blockers (not just the
                // first) and doing so even when pollers are queued is what breaks the
                // record-mode pipe livelock: previously a queued poller made this branch
                // return early forever, so the completed read/write was never rescheduled
                // and the poller spun on data that never arrived.
                if !ready.is_empty() {
                    for ready_dtid in &ready {
                        info!(
                            "[step2] Reschedule formerly (external IO) blocked dtid {:?}",
                            ready_dtid
                        );
                        self.blocked.external_io_blockers.remove(ready_dtid);
                        self.run_queue.push_eager_io_repoll(*ready_dtid);
                    }
                    return Ok(());
                }

                // No completed IO yet, and nothing but pollers (or nothing) to run. The
                // blocking syscalls are executing in the host kernel; go around the loop
                // and re-check readiness. (Still a busy-wait; see T137183027 for the
                // record-the-nondeterministic-event fix.)
                trace!(
                    "[step2] eagerly waiting on external IO for dtids {:?}. spinning.",
                    &self.blocked.external_io_blockers
                );
                std::thread::yield_now();
                return Err(SkipTurn);
            } // End region which should be deleted.

            // Use the same deterministic-work-first policy as record/replay. Host
            // completion timing must not decide whether a ready continuation overtakes
            // guest work that was already runnable. Pollers are excluded because they
            // commonly wait for the completed operation and would otherwise starve it.
            let only_pollers = if let Some(fp) = self.run_queue.first_priority() {
                fp >= LAST_PRIORITY
            } else {
                true
            };
            if !self.run_queue.is_empty() && !only_pollers {
                return Ok(());
            }

            if !ready.is_empty() {
                for ready_dtid in &ready {
                    info!(
                        "[step2] Reschedule formerly (external IO) blocked dtid {:?}",
                        ready_dtid
                    );
                    self.blocked.external_io_blockers.remove(ready_dtid);
                    self.run_queue.push_eager_io_repoll(*ready_dtid);
                }
            }
            if self.run_queue.is_empty()
                && self.blocked.timed_waiters.is_empty()
                && !self.blocked.external_io_blockers.is_empty()
            {
                // TODO (T137184765): for now we just WAIT eagerly whenever there is blocking
                // external IO and else to do. We implement a busy-wait by going around the
                // scheduler loop again.
                trace!(
                    "[step2] TEMPORARY2: eagerly blocking on external IO for dtids {:?}.  SPINNING!",
                    &self.blocked.external_io_blockers
                );
                std::thread::yield_now();
                Err(SkipTurn)
            } else {
                // Productive work to do, irrespcetive of what's blocked, so let's get to it.
                Ok(())
            }
        } else {
            Ok(())
        }
    }

    fn step2d_handle_empty_queue(
        &mut self,
        global_time: &Arc<Mutex<GlobalTime>>,
    ) -> Result<(), SkipTurn> {
        let timed_empty = self.blocked.timed_waiters.is_empty();
        let blockers_empty = self.blocked.external_io_blockers.is_empty();
        let futex_empty = self.blocked.no_futex_waiters();

        if self.run_queue.is_empty() {
            if !self.pending_physical_process_exits.is_empty() {
                // The SaBRe plugin has run the child process's logical exit hook, but the ptrace
                // supervisor has not received its final wait status. Fast-forwarding the next
                // timer here can fire a parent's timeout before the child becomes waitable.
                trace!(
                    "waiting for physical process exits before empty-queue timer fast-forward: {:?}",
                    self.pending_physical_process_exits
                );
                std::thread::yield_now();
                return Err(SkipTurn);
            }
            // When the run queue is empty, we sometimes need to give things a kick.
            if futex_empty && timed_empty && blockers_empty {
                info!("scheduler (step2_process_blocked): zero threads left anywhere, fizzling.");
                return Err(SkipTurn);
            } else if !futex_empty && timed_empty && blockers_empty {
                panic!(
                    "Deadlock detected: thread(s) waiting on futex, but no runnable threads left.\n \
                 queue: {:?}\n  next_turns: {:?}\n  blocked: {:?} \n",
                    self.run_queue, self.next_turns, self.blocked
                )
            } else if !timed_empty {
                debug!(
                    "[scheduler] Deadlock avoidance! Empty run-queue, so waking next timed event."
                );
                let (event_ns, evt) = self
                    .blocked
                    .timed_waiters
                    .pop()
                    .expect("internal error: no timed events found");
                info!("[scheduler] Skipping global time ahead to {}.", event_ns);
                {
                    let mut gt = global_time.lock().unwrap();
                    let gt_now_ns = gt.as_nanos();
                    let delta = event_ns.duration_since(gt_now_ns);
                    detlog_debug!(
                        "[sched] add extra global time for deadlock avoidance {:?} on current time {}",
                        delta,
                        gt_now_ns,
                    );
                    gt.add_extra_time(delta);
                }

                match evt {
                    TimedEvent::ThreadEvt(dtid) => self.wake_timed_event(event_ns, dtid),
                    TimedEvent::SignalEvt(id, dtid, sig) => {
                        self.fire_alarm(id.process(), dtid, sig)
                    }
                }
                return Err(SkipTurn);
            }
        }
        Ok(())
    }

    /// Step: Find the next thread to run for this scheduling run.
    /// Sometimes the next thread is from the run queue, but it can also be a timed event.
    /// Return `None` if the queue is empty.
    ///
    /// This is a "peek" in the sense that it leaves the thread in the run queue.
    fn step3_peek(&mut self) -> Option<(DetTid, Ivar<SchedRequest>, Ivar<SchedResponse>)> {
        // Re-admit any happens-before threads whose gate opened since last turn.
        // Must precede `tentative_pop_next`: the run queue forbids pushes while a
        // selection transaction is live.
        self.hb_flush_wakes();
        debug!(
            "[sched-step3] Stepping scheduler, queue len {}, current turn {}, committed_time {}",
            self.run_queue.len(),
            self.turn,
            self.committed_time
        );

        // Enable for FULL detail:
        {
            trace!(
                "[sched-step3] queue {:?}, io-blocked {:?}, next_turns: ",
                &self.run_queue, self.blocked.external_io_blockers
            );
            for (dtid, nxt) in self.next_turns.iter() {
                trace!(" ==> dtid {}, req {}, resp {}", dtid, nxt.req, nxt.resp);
            }
            if !self.blocked.timed_waiters.is_empty() {
                trace!("Timed events: {:?}", self.blocked.timed_waiters);
            }
        }

        if self.run_queue.is_empty() {
            None
        } else {
            let next_dtid = if self.vfork_barriers.is_empty() {
                self.run_queue.tentative_pop_next().expect("impossible")
            } else {
                let child = self
                    .vfork_barriers
                    .values()
                    .flatten()
                    .find(|child| self.run_queue.contains_tid(**child))
                    .copied()?;
                self.run_queue
                    .tentative_pop_tid(child)
                    .expect("vfork child disappeared from run queue")
            };
            let nextturn = self.next_turns.get(&next_dtid).unwrap_or_else(|| {
                panic!(
                "[sched-step3] internal error: dettid {} queued but missing entry in next_turns",
                    next_dtid
            )
            });
            Some((next_dtid, nextturn.req.clone(), nextturn.resp.clone()))
        }
    }

    /// Deschedule, but do not clear request/response. This should be used when
    /// the turn was skipped because the blocked-on resource is still blocking.
    fn skip_turn_blocked(&mut self, dettid: DetTid) -> Result<(), SkipTurn> {
        self.run_queue.undo_tentative_pop(); // Started in step3.
        assert!(self.run_queue.remove_tid(dettid)); // Deschedule while we wait.
        trace!(
            "[dtid {}] after removal, run queue: {:?}",
            dettid, &self.run_queue
        );
        self.skip_turn()
    }

    /// Post-await re-check: the selected thread parked (its request resolved
    /// `Ok`), but did its `next_turns` entry survive until the daemon got the
    /// lock back? If not, the turn must be abandoned.
    ///
    /// Returns `Err(SkipTurn)` for the abandoned case, after closing the
    /// tentative window `step3_peek` opened. Two things hang on that `Err`:
    ///
    /// * The tentative pop is undone rather than committed, so the selection
    ///   does not outlive the turn and the next pass's step2 removal drain does
    ///   not call `remove_tid` against a live `tentative_selection` (the same
    ///   hygiene the `Err(ThreadExited)` arm needs).
    /// * The caller reports a SKIP, not a completed turn. This branch bypasses
    ///   steps 4-7, so nothing is blocked, unblocked or re-enqueued -- yet
    ///   `bump_global_time` suppresses its advance only on `last_turn.is_err()`
    ///   ("if the last turn was a skip, it shouldn't really have time-bumped").
    ///   Reporting `Ok` here therefore added a DETLOG-visible virtual-time tick
    ///   for work that never happened, and whether this branch is reached at all
    ///   depends on whether teardown cleared `next_turns` inside the host-timed
    ///   gap between the await resolving and the re-lock -- so the same logical
    ///   execution could gain that tick in one run and not the next.
    fn abort_turn_if_thread_vanished(&mut self, next_dtid: DetTid) -> Result<(), SkipTurn> {
        if self.next_turns.contains_key(&next_dtid) {
            return Ok(());
        }
        info!(
            "[sched-daemon] thread {} exited, skipping over...",
            &next_dtid
        );
        self.run_queue.undo_tentative_pop();
        Err(SkipTurn)
    }

    /// Simply advance the turn. This does NOT remove any threads from the
    /// runqueue; callers must maintain `run_queue`/`blocking` invariants.
    fn skip_turn(&mut self) -> Result<(), SkipTurn> {
        self.turn += 1; // Skipping the turn advances the turn.
        Err(SkipTurn)
    }

    /// Step: Determine if action will block based on current information.  E.g. will it block
    /// on a pipe read with no writer? If so, register it in the blocked_pool and issue a "skip".
    /// We can go ahead and take resource locks and physically issue the blocking effect if we
    /// like.  It's immaterial whether we do that now or later.
    ///
    /// Postcondition:
    ///  - If returning SkipTurn, this function ENDS the Scheduler turn, advancing to the
    ///    next (skipping subsequent steps within theturn).  Otherwise, it waits for a
    ///    later step end the turn.
    #[allow(clippy::unnecessary_wraps)]
    fn step4_resource_block(
        &mut self,
        dettid: DetTid,
        rs: &Resources,
        resp: &Ivar<SchedResponse>,
    ) -> Result<(), SkipTurn> {
        if rs.poll_attempt > 0 {
            // The thread is polling and hasn't been "remade" as runnable yet.
            info!(
                "[scheduler] >>>>>>>\n\n NONCOMMIT turn {}, SKIP dettid {} polling resource {:?}",
                self.turn, dettid, rs
            );
            // Requeue the thread as a poller
            let popped = self.run_queue.commit_tentative_pop();
            assert_eq!(dettid, popped);
            self.run_queue
                .push_poller(dettid, self.get_priority(dettid), rs.poll_attempt);
            trace!(
                "[dtid {}] after deprioritizing polling request, run queue: {:?}",
                dettid, &self.run_queue
            );
            self.upgrade_polled_to_runnable(dettid, rs); // Indicate the thread gets to run next time
            self.skip_turn()
        } else {
            match rs.resources.len() {
                0 => Ok(()),
                1 => {
                    let (rid, perm) = rs.resources.iter().next().unwrap();
                    self.block_for_one_resource(dettid, rid, perm, resp)
                }
                _ => {
                    panic!(
                        "Requests for more than one resource at a time are not supported yet: {:?}",
                        rs
                    )
                }
            }
        }
    }

    /// Replace the request Ivar for `dettid` with a copy with `poll_attempt = 0`,
    /// indicating the poll request is runnable on the next trip through the run queue.
    ///
    /// Precondition: The guest is stopped, so that no one is potentially using the request Ivar.
    /// The request Ivar should also be full with the passed resources
    fn upgrade_polled_to_runnable(&mut self, dettid: DetTid, rs: &Resources) {
        let mut retry_rs = rs.clone();
        retry_rs.poll_attempt = 0;
        let runnable_req = Ivar::full(Ok(retry_rs));
        let req = &mut self
            .next_turns
            .get_mut(&dettid)
            .expect("nextturn present")
            .req;
        debug_assert!(req.try_read().unwrap().is_ok()); // Ivar should be full
        trace!(
            "[dtid {}] Upgrading polled resource request in {} to runnable non-polled in {}",
            dettid, req, runnable_req
        );
        *req = runnable_req;
    }

    /// Helper function. Same postcondition as step4_resource_block
    fn block_for_one_resource(
        &mut self,
        dettid: DetTid,
        rid: &ResourceID,
        _perm: &Permission,
        resp: &Ivar<SchedResponse>,
    ) -> Result<(), SkipTurn> {
        match rid {
            ResourceID::SleepUntil(target_ns) => {
                if *target_ns <= self.committed_time {
                    trace!(
                        "[dtid {}] time-based action ready to execute, target time {} is before committed global time {}",
                        dettid, target_ns, self.committed_time
                    );
                    Ok(())
                } else {
                    trace!(
                        "[dtid {}] time-based action not ready yet, registering waiter at future time {}. Current time is {}",
                        dettid, target_ns, self.committed_time
                    );
                    info!(
                        "[scheduler] >>>>>>>\n\n NONCOMMIT turn {}, SKIP dettid {} which wanted resource {:?} (blocking)",
                        self.turn, dettid, rid
                    );
                    self.blocked.timed_waiters.insert(*target_ns, dettid);
                    self.skip_turn_blocked(dettid)
                }
            }

            // Thread BEGINS [potentially] blocking external IO
            ResourceID::BlockingExternalIO(op_id) | ResourceID::BlockingVfork(op_id) => {
                if matches!(rid, ResourceID::BlockingVfork(_)) {
                    assert!(self.vfork_barriers.insert(dettid, None).is_none());
                }
                info!(
                    "[scheduler] >>>>>>>\n\n COMMIT turn {}, BACKGROUND dettid {} (maybe-blocking)",
                    self.turn, dettid
                );
                // Here we allow the action to execute asynchrounously, in the
                // background. The protocol is that it must:
                //   (1) not interfere with other internal/external actions (independence),
                //   (2) Request a BlockedExternalContinue as the first thing after the external IO is complete.
                self.run_queue.undo_tentative_pop(); // Begun in step3
                assert!(self.run_queue.remove_tid(dettid)); // Deschedule while in background.

                // TODO: Register the action that is occuring in the background:
                // let act = self.new_action(Ivar::new());
                // self.bg_action_pool.insert(act.action_id, act);

                // Unblock guest so that potentially-blocking IO action can get
                // started. This intentionally races with subsequent turns the
                // scheduler commits, and thus it leans on an assumption of
                // non-interference, or on interference *only* affecting the external
                // actions that will be recorded anyway.
                self.run_queue.consume_yield_exclusion();
                self.unblock_guest(dettid, resp);

                // Only once the ivars are cleared, and the guest is officially past the
                // BlockingExternalIO phase ready to issue BlockedExternalContinue, do we
                // then put it into the external_io_blockers struct.
                let old = self.blocked.external_io_blockers.insert(dettid, *op_id);
                assert!(old.is_none(), "thread started a second external operation");
                Err(SkipTurn)
            }

            // Thread CONTINUES after completing [potentially] blocking IO.
            ResourceID::BlockedExternalContinue(_) | ResourceID::VforkFailed(_) => {
                // We leave the thread out of the run-queue.  At the point we put it back
                // in, this resource request is immediately granted.
                Ok(())
            }

            // Thread requests change in priority
            ResourceID::PriorityChangePoint(prio, change_time, rcbs, epochs) => {
                self.perform_priority_changepoint(dettid, *prio, *change_time, *rcbs, epochs)
            }

            // For now, all other resource types are immediately granted.
            // (TODO/FIXME: handle the entire set of resource requests.)
            ResourceID::FileContents(_) => Ok(()),
            ResourceID::FileMetadata(_) => Ok(()),
            ResourceID::DirectoryContents(_) => Ok(()),
            ResourceID::MemAddrSpace(_) => Ok(()),
            ResourceID::Path(_) => Ok(()),
            ResourceID::PathsTransitive(_) => Ok(()),
            ResourceID::Device(_) => Ok(()),
            // The scheduler-ordered `Exit` grant is the deterministic moment a
            // child process leaves the run set. Register a one-shot child-exit
            // `SIGCHLD` for the reaping parent, to be delivered at a deterministic
            // logical time by `step2b_process_timed`, instead of relying on the
            // host-async kernel `SIGCHLD` whose arrival time is host-timed (the
            // `make -jN` / redis `--strict --verify` nondeterminism source).
            ResourceID::Exit { group, process, .. } => {
                if *group && let Some(parent) = self.thread_tree.parent_process(process) {
                    // Fire strictly after the current committed time so the event
                    // is dispatched on a subsequent scheduler pass (DetTid == DetPid
                    // for a group leader, so `parent` is also the parent thread id).
                    let deadline = self.committed_time + LogicalTime::from_nanos(1);
                    self.blocked
                        .timed_waiters
                        .insert_child_exit(deadline, *process, parent, parent);
                }
                Ok(())
            }
            ResourceID::ParentContinue { .. } => Ok(()),
            ResourceID::InternalIOPolling => Ok(()),
            ResourceID::FutexWait => Ok(()),
            ResourceID::TraceReplay => Ok(()),
            ResourceID::SchedYield => Ok(()),

            // A guest thread checking in at a happens-before anchor point. Delegate
            // to the enforcement logic, which either grants passage (firing anchors)
            // or parks the thread until its gating BEFORE anchor fires.
            ResourceID::HappensBeforeCheckpoint(count) => self.hb_checkpoint(dettid, *count),

            // A host-async SIGCHLD (a guest child process exited) is delivered to
            // the parent at a moment decided by host timing. Committing that turn
            // immediately makes the signal race whatever guest work was already
            // runnable (e.g. a `make -jN` jobserver `pselect6` continuation),
            // which diverges under `--strict --verify`. Defer it deterministic-
            // work-first: park the parent out of the run queue and let
            // `step2e_process_signal_deferred` re-admit it once no ordinary guest
            // work remains, mirroring the `external_io_blockers` policy. Signals
            // that the scheduler itself synthesizes deterministically (timers via
            // `fire_alarm`) are never SIGCHLD and are unaffected.
            ResourceID::InboundSignal(SigWrapper(sig)) => {
                // `sigchld_ready` marks a parent step2e has already re-admitted;
                // grant it now rather than deferring it a second time.
                let already_readmitted = self.blocked.sigchld_ready.remove(&dettid);
                if *sig == Signal::SIGCHLD
                    && !already_readmitted
                    && self.run_queue.has_runnable_besides(dettid)
                {
                    self.run_queue.undo_tentative_pop(); // Begun in step3.
                    assert!(self.run_queue.remove_tid(dettid));
                    self.blocked.sigchld_deferred.insert(dettid);
                    Err(SkipTurn)
                } else {
                    Ok(())
                }
            }
        }
    }

    // TODO-HUMAN-REVIEW(PR-868): Review the vfork registration scheduler barrier.
    pub(crate) fn complete_vfork_registration(&mut self, parent: DetTid, child: DetTid) {
        let registered_child = self
            .vfork_barriers
            .get_mut(&parent)
            .unwrap_or_else(|| panic!("vfork child registered without a pending parent {parent}"));
        assert!(registered_child.replace(child).is_none());
    }

    /// Inner helper for just the core priority changing.
    fn requeue_with_new_priority(&mut self, dettid: DetTid, new_priority: Priority) {
        // TODO: do we want to record in preemption_writer if we are in schedule-trace-replay mode?
        assert!(runqueue::is_ordinary_priority(new_priority));
        // Alter the threads priority and requeue.
        let _old_priority = self.priorities.insert(dettid, new_priority);
        let present = self.run_queue.remove_tid(dettid);
        if present {
            self.runqueue_push_back(dettid); // Repush with new priority
        }
        trace!(
            "[dettid {}] requeue: Priority mapping after change to priority {}: {:?}",
            dettid, new_priority, self.priorities
        );
    }

    /// Helper for priority changepoint logic
    ///
    /// Precondition: guest is stopped so that there is no chance the ivars are being used
    /// concurrently while they are being cleared.
    ///
    /// Postcondition: Same as block_for_one_resource. However, always returns SkipTurn, because the
    /// priority changepoint may not allow the current thread to continue in a regular turn (i.e.
    /// doing actual work).
    fn perform_priority_changepoint(
        &mut self,
        dettid: DetTid,

        new_priority: Priority,
        guest_time: LogicalTime,
        guest_rcbs: u64,
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(PR-1151)
        chaos_epochs: &[crate::resources::ChaosEpochTransition],
    ) -> Result<(), SkipTurn> {
        assert!(runqueue::is_ordinary_priority(new_priority));
        // Alter the threads priority and requeue.
        let old_priority = self.priorities.insert(dettid, new_priority);

        // Do not attempt to record preemptions/priorities when we're dictated by a raw schedule replay.
        if self.replayer.is_none()
            && let Some(pw) = &mut self.preemption_writer
        {
            let old_prio = old_priority.unwrap();
            debug!(
                "[dtid {}] Recording preemption point, current time {} prior priority {} (next priority {})",
                dettid, guest_time, old_prio, new_priority
            );
            for transition in chaos_epochs {
                pw.insert_chaos_epoch(dettid, *transition);
            }
            pw.insert_reprioritization(dettid, guest_time, guest_rcbs, old_prio, new_priority);
            pw.set_current(dettid, new_priority);
        }

        let popped = self.run_queue.commit_tentative_pop(); // Begun in step3.
        assert_eq!(dettid, popped);
        self.runqueue_push_back(dettid); // Repush with new priority
        trace!(
            "[dettid {}] changepoint: Priority mapping after change to priority {}: {:?}",
            dettid, new_priority, self.priorities
        );

        // Update request to be empty so the thread is unconditionally
        // runnable when it next comes up in the queue.
        let empty_req = Ivar::full(Ok(Resources::new(dettid)));
        trace!(
            "[dettid {}] Priority change point emplaced empty resource request at new {}",
            dettid, empty_req
        );
        self.next_turns
            .get_mut(&dettid)
            .expect("nextturn present")
            .req = empty_req;
        info!(
            "[scheduler] >>>>>>>\n\n NONCOMMIT turn {}, dettid {} changed priority to {}",
            self.turn, dettid, new_priority
        );
        self.skip_turn() // The thread shouldn't run.
    }

    /// Step1: Wait till threads park. Also tick global logical time due to the scheduler itself.
    ///
    /// N.B. Currently, as an overapproximation, we check for full quiescence!
    ///
    /// N.B. This was formerly "step 3" and has been temporarily moved earlier to make
    /// things easier for the time being.
    fn step1_check_quiescence(
        &mut self,
        global_time: &Mutex<GlobalTime>,
        last_turn: &Result<Resources, SkipTurn>,
    ) -> Option<Ivar<SchedRequest>> {
        // TODO: actually check resource availability to enable asynchronous background activities!
        let outstanding = self.are_all_quiesced();
        if outstanding.is_none() {
            self.bump_global_time(global_time, last_turn);
        }
        outstanding
    }

    fn is_internal_turn(rsrcs: &Resources) -> bool {
        Self::is_x_turn(rsrcs, &ResourceID::TraceReplay)
    }

    /// A turn that only grants `InternalIOPolling`, i.e. a retry of a nonblocking
    /// poll/epoll/select/futex/recv/... injected by the blocking-via-polling machinery
    /// (see `retry_nonblocking_syscall_helper`). The *number* of such retries before a
    /// file descriptor becomes ready is wall-clock dependent when the readiness is driven
    /// by an external actor (e.g. a child linker process draining a pipe), so it varies
    /// between otherwise-identical runs.
    fn is_polling_turn(rsrcs: &Resources) -> bool {
        Self::is_x_turn(rsrcs, &ResourceID::InternalIOPolling)
    }

    /// SaBRe discovers an inherited stdio pipe as a device resource before the inner
    /// `InternalIOPolling` request. Both turns belong to one host-timing-sensitive pipe
    /// operation, so their logical-time logging must use the same retry normalization.
    fn is_sabre_internal_pipe_io_turn(&self, rsrcs: &Resources) -> bool {
        rsrcs.fyi == SABRE_INTERNAL_PIPE_IO_FYI
    }

    /// A strong yield issued by a SaBRe task before a zero-timeout poll while it owns a
    /// loopback connection. Its count is kernel-readiness timing, not guest-visible progress.
    fn is_sabre_loopback_poll_yield_turn(&self, rsrcs: &Resources) -> bool {
        rsrcs.fyi == SABRE_LOOPBACK_POLL_YIELD_FYI
    }

    fn is_x_turn(rsrcs: &Resources, x: &ResourceID) -> bool {
        if rsrcs.resources.contains_key(x) {
            if rsrcs.resources.len() > 1 {
                panic!(
                    "is_x_turn: not expecting an {:?} mixed in with other resource requests: {:?}",
                    x, rsrcs
                );
            }
            true
        } else {
            false
        }
    }

    /// Tick global logical time due to represent the work of the scheduler itself.
    /// Also, update committed time.
    /// Prerequisite: all threads are parked, with their time contributions frozen.
    fn bump_global_time(
        &mut self,
        global_time: &Mutex<GlobalTime>,
        last_turn: &Result<Resources, SkipTurn>,
    ) {
        // An internal IO-polling retry (see `is_polling_turn`) must still advance logical
        // time -- finite poll/epoll/select/futex timeouts are enforced by comparing observed
        // logical time against the deadline in `retry_nonblocking_syscall_helper`, so freezing
        // time here would turn a timed wait into an infinite spin. But because the *count* of
        // these retries is host-timing nondeterministic, we keep their time-advance out of the
        // determinism log (DETLOG). This makes the `--verify` deterministic comparison
        // insensitive to retry count; the matching `{InternalIOPolling: ...}` COMMIT turn is
        // likewise excluded in `logdiff::is_internal_io_poll_commit`. Time values still shift
        // between runs, but those are numerically normalized before comparison.
        let last_turn_was_polling = last_turn
            .as_ref()
            .map(|resources| {
                Self::is_polling_turn(resources)
                    || self.is_sabre_internal_pipe_io_turn(resources)
                    || self.is_sabre_loopback_poll_yield_turn(resources)
            })
            .unwrap_or(false);

        // At this moment, when threads are parked, we know that the global_time is
        // frozen and we can read it without any race.
        let snapshot: LogicalTime = {
            let mut gtime = global_time.lock().unwrap();

            if self.run_queue.is_empty() && self.blocked.only_external_blocked() {
                // TODO(T112017687): rationalize the occurence of
                // BlockingExternalIO in strict runs. For example, we should
                // probably inject nanosleep and actually wait the intervening
                // time, so we don't appear too fast to external observers.
                trace!(
                    "[scheduler] skipping scheduler time advance because we're ONLY waiting for external events"
                );
            } else if last_turn.is_err() {
                // Note: if the last turn was a skip, it shouldn't really have time-bumped. But since we
                // can't see the future, we just cancel out the bump by not doing a bump this turn.
                trace!(
                    "[scheduler] skipping scheduler time advance because just-finished turn did not progress (i.e. SkipTurn)"
                );
            } else if last_turn
                .as_ref()
                .map(Self::is_internal_turn)
                .unwrap_or(false)
            {
                trace!(
                    "[scheduler] skipping scheduler time advance because just-finished turn was an internal book-keeping one"
                );
            } else {
                let newtime = gtime.add_scheduler_time();
                if last_turn_was_polling {
                    // Advance time (needed for timeout enforcement) but keep it off the DETLOG.
                    trace!(
                        "[sched] advance global time for internal IO-polling retry (suppressed from detlog), new time {:?}",
                        newtime,
                    );
                } else {
                    detlog_debug!(
                        "[sched] advance global time for scheduler turn, new time {:?}",
                        newtime,
                    );
                }
            }
            gtime.as_nanos()
        };

        match snapshot.cmp(&self.committed_time) {
            std::cmp::Ordering::Less => {
                panic!(
                    "bump_global_time: invariant broken, global time went backwards from {} to {}",
                    self.committed_time, snapshot
                );
            }
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Greater => {
                // NB: `committed_time` still tracks the (host-timing-perturbed) global clock,
                // including the time advanced by suppressed IO-polling retries above, so this
                // line's presence is retry-count sensitive. It is therefore excluded from the
                // deterministic `--verify` comparison in `logdiff::is_scheduler_committed_time`
                // (it is redundant with the per-turn "advance global time" DETLOG anyway).
                detlog_debug!(
                    "[sched-step1] advancing committed_time from {} to {}",
                    self.committed_time,
                    snapshot
                );
                self.committed_time = snapshot;
            }
        }
    }

    /// Step 4: unblock enabled actions to actually, physically run.
    fn step5_guest_unblock(
        &mut self,
        next_dtid: DetTid,
        rsrcs: &Resources,
        resp: &Ivar<SchedResponse>,
    ) -> Result<(), SkipTurn> {
        match self.next_turns.get(&next_dtid) {
            None => {
                info!(
                    "Scheduler was about to schedule {} for a turn (resources {:?}), but it died first.",
                    &next_dtid, rsrcs.resources
                );
                Err(SkipTurn)
            }
            Some(nxt) => {
                assert_eq!(resp, &nxt.resp);
                // N.B.: these prints themselves should be deterministic between
                // runs.  They are part of the "detlog".
                let normalization_marker = if self.is_sabre_internal_pipe_io_turn(rsrcs) {
                    " [sabre-internal-pipe-io]"
                } else if self.is_sabre_loopback_poll_yield_turn(rsrcs) {
                    " [sabre-loopback-poll-zero-timeout]"
                } else {
                    ""
                };
                info!(
                    "[sched-step5] >>>>>>>\n\n COMMIT turn {}, dettid {} using resources {:?}, on previously committed {}{}",
                    self.turn,
                    next_dtid,
                    rsrcs.resources,
                    self.committed_time,
                    normalization_marker,
                );
                self.unblock_guest(next_dtid, resp);
                Ok(())
            }
        }
    }

    /// Unblock the guest to run, clear its ivars for the next turn, and increment the turn counter.
    ///
    /// Precondition: guest is stopped.
    /// Postcondition: guest is running concurrently with this scheduler/tracer thread.
    fn unblock_guest(&mut self, dtid: DetTid, resp: &Ivar<SchedResponse>) {
        self.turn += 1;
        trace!(
            "[sched-step5] Guest unblocking (via {}); clear ivars for the next turn on dettid {}",
            &resp, &dtid
        );
        let sig = self.is_signal_inbound(dtid); // Peek before we clear the ivars.
        let futex_timed_out = self.blocked.timed_out_futex_waiters.remove(&dtid);
        self.clear_nextturn(dtid);
        let answer = if sig {
            SchedResponse::Signaled()
        } else if futex_timed_out {
            SchedResponse::Go(Some(SchedValue::TimeOut))
        } else {
            let timeslice = self.timeslices.remove(&dtid).flatten();
            // TODO(T137799529): use a more strongly typed representation rather than reusing
            // SchedValue/u64:
            let as_schedvalue = timeslice
                .as_ref()
                .map(LogicalTime::as_nanos)
                .map(SchedValue::Value);
            SchedResponse::Go(as_schedvalue)
        };
        resp.put(answer);
    }

    fn is_signal_inbound(&self, dettid: DetTid) -> bool {
        let req = &self.next_turns.get(&dettid).unwrap().req;
        if let Some(Ok(rsrcs)) = req.try_read() {
            for rsrc in rsrcs.resources.iter() {
                if let (ResourceID::InboundSignal(_), _) = rsrc {
                    return true;
                }
            }
            false
        } else {
            false
        }
    }

    /// Clear the thread's nextturn, installing fresh ivars.
    ///
    /// Precondition: guest is stopped so that there is no chance the ivars are being used
    /// concurrently while they are being cleared.
    fn clear_nextturn(&mut self, dtid: DetTid) {
        let nextturn = self
            .next_turns
            .get_mut(&dtid)
            .expect("clear_nextturn: Thread should be available in next_turns");
        nextturn.req = Ivar::new();
        nextturn.resp = Ivar::new();
    }

    /// Step: reenqueue the thread that just had a turn.
    fn step6_reenquue(&mut self, next_dtid: DetTid, sched_yield: bool) {
        // We delay popping till here, so while holding the lock we "atomically" move the
        // thread from the front to the back of the queue.
        let dt2 = self.run_queue.commit_tentative_pop_completed_turn();
        assert_eq!(next_dtid, dt2);
        // SchedYield is emitted in normal execution and non-chaos preemption replay. Its
        // queue placement is transient, so persistent priorities remain unchanged.
        let pos = if sched_yield {
            let priority = self.get_priority(next_dtid);
            self.run_queue.push_yielded(next_dtid, priority)
        } else {
            self.runqueue_push_back(next_dtid)
        };
        debug!(
            "[sched-step6] dettid {} going back into queue at position {}.",
            next_dtid, pos
        );
    }

    /// Add a simulated "post hook" for exit calls which we're about to let through.
    /// ALTERNATIVE: this could happen later when the thread_exit hook comes through.
    fn step7_simulate_exit_posthook(
        &mut self,
        dettid: DetTid,
        placeholder_syscall: Syscall,
        global_time: &Mutex<GlobalTime>,
    ) {
        let replay = self.replayer.is_some();
        let record = self.preemption_writer.is_some();
        if !(replay || record) {
            return;
        }
        let thread_duration = global_time.lock().unwrap().threads_duration(dettid);
        debug!(
            "simulate exit posthook on tid {}, thread time {}: {:?}",
            dettid, thread_duration, placeholder_syscall
        );

        let ev = SchedEvent::syscall(dettid, placeholder_syscall.number(), SyscallPhase::Posthook)
            .with_time(thread_duration);
        let print_stack1 = if replay {
            let ConsumeResult {
                keep_running,
                print_stack,
                event_ix: _,
                timeslice_remaining: _,
            } = self.consume_schedevent(&ev);
            // We should not ever need to background the thread when it is going to exit anyway.
            if !keep_running {
                tracing::warn!(
                    "simulate_exit_posthook: unexpectedly asked to background the current, exiting thread {}",
                    dettid
                );
            }
            print_stack
        } else {
            None
        };
        let print_stack2 = if record { self.record_event(&ev) } else { None };
        if print_stack1.is_some() || print_stack2.is_some() {
            eprintln!(
                ":: Guest tid {}, at thread time {}, backtrace requested but not available post-exit!\n",
                dettid, thread_duration
            );
        }
    }

    /// Get the priority for a thread; panic if absent.
    fn get_priority(&self, dettid: DetTid) -> Priority {
        *self
            .priorities
            .get(&dettid)
            .expect("get_priority: all threads should have a persistent priority")
    }

    /// Push_back a thread onto the runqueue, respecting its persistent priority
    /// value. This should be the ordinary way threads are pushed onto the queue.
    pub fn runqueue_push_back(&mut self, dettid: DetTid) -> PrioritizedOrder {
        let priority = self.get_priority(dettid);
        self.run_queue.push_back(dettid, priority)
    }

    /// Push a thread to the front of its persistent priority band.
    ///
    /// This is reserved for protocol handoffs where the queued thread must run
    /// before an equal-priority peer, such as ordinary clone child startup.
    pub(crate) fn runqueue_push_front(&mut self, dettid: DetTid) -> PrioritizedOrder {
        let priority = self.get_priority(dettid);
        self.run_queue.push_front(dettid, priority)
    }

    /// Record an intent to admit `dtid` to the run queue, applied by the daemon
    /// at the next deterministic drain point ([`step2`](Self::step2_process_blocked)).
    ///
    /// Global-request handlers (`recv_create_child_thread`,
    /// `reconnect_after_exec`) hold the scheduler lock but run on whichever
    /// backend worker fielded the RPC, not on the scheduler daemon's turn. The
    /// point in the daemon's loop at which such a handler acquires the lock is
    /// host-timing-dependent on asynchronous backends (e.g. DBI): it may land
    /// inside the tentative-pop window (between `step3_peek` and `step4`'s
    /// commit, where the lock is released across `req.get().await`) *or* outside
    /// it (during the quiescence-wait / backoff awaits at the top of
    /// `do_a_turn_blocking`, where `tentative_selection` is `None`). A design
    /// that pushed directly whenever the window happened to be closed would make
    /// the *admission order* — and, under `RunsPostFork::Random`, the PRNG draw
    /// order — a function of that host timing: two equal-priority admissions
    /// could enter the queue in either relative order across otherwise-identical
    /// runs, and a fixed seed could explore different schedules.
    ///
    /// So admission is *always* deferred, never applied directly here. Handlers
    /// only record the unresolved [`AdmitIntent`]; the daemon resolves the side
    /// (drawing any `RunsPostFork::Random` value) and pushes the run queue at the
    /// single `step2` drain, in canonical `DetTid` order, before `step3` opens a
    /// tentative window. Draining is also the only place `remove_tid`'s tentative
    /// guard is guaranteed to hold.
    ///
    /// # What this does and does not make deterministic
    ///
    /// **Synchronous backends (ptrace): fully deterministic, and byte-identical
    /// to the pre-deferral behavior.** Handlers run post-commit, one per turn, so
    /// at most one admission is buffered per turn and it drains at the next
    /// `step2` — before that turn's `step3` selection — yielding the same
    /// selection sequence and the same one-draw-per-fork PRNG order as an
    /// immediate push.
    ///
    /// **Asynchronous backends: every production admission site has a causal or
    /// explicit barrier that fixes its drain, and order within that drain is
    /// canonical.** A bare off-turn handler would still be insufficient: a
    /// `BTreeMap` only canonicalizes items already in one snapshot. The current
    /// sites additionally bind snapshot membership to deterministic scheduler
    /// state:
    ///
    /// * **Ordinary clone — anchored, causally.** `CreateChildThread` issues the
    ///   parent's `ParentContinue` request only *after* buffering the child's
    ///   admission, so no thread can run between the two and the admission
    ///   cannot straddle a drain boundary.
    /// * **`vfork` — anchored, by barrier.** `vfork_barriers` /
    ///   [`Scheduler::step2a_wait_for_vfork_barrier`] hold the parent until the
    ///   child has registered, which fixes the drain.
    /// * **Multi-threaded exec reconnect — anchored, causally.** Step5 installs
    ///   the caller's empty next-turn request before it executes exec. The
    ///   reconnect handler atomically buffers the old-leader removal and new
    ///   incarnation admission, then retires the caller and resolves that
    ///   request. Step1 therefore cannot release step2 before the complete pair
    ///   exists. [`Scheduler::replace_retired_run_queue_incarnation`] binds the
    ///   same-raw-TID handoff explicitly.
    ///
    /// Thus both membership and within-drain resolution are functions of
    /// deterministic state for all current sites. The exec regression test
    /// forces the daemon to wait before reconnect, varies host yields, and
    /// compares the exact first-drain queue plus the next post-fork PRNG draw.
    pub(crate) fn admit_to_run_queue(&mut self, dtid: DetTid, intent: AdmitIntent) {
        let prev = self.pending_run_queue_admissions.insert(dtid, intent);
        debug_assert!(
            prev.is_none(),
            "thread {:?} recorded for run-queue admission twice before draining",
            dtid
        );
    }

    /// Atomically classify a same-raw-TID exec handoff and record its fresh
    /// admission. The scheduler mutex serializes this method with `step2`, so a
    /// drain can never observe only one half of the handoff.
    fn replace_retired_run_queue_incarnation(&mut self, dtid: DetTid, intent: AdmitIntent) {
        let disposition = self
            .pending_run_queue_removals
            .get_mut(&dtid)
            .unwrap_or_else(|| {
                panic!(
                    "exec replacement {:?} has no retired run-queue incarnation",
                    dtid
                )
            });
        assert_eq!(
            *disposition,
            RemovalDisposition::Retire,
            "exec replacement {:?} was classified more than once",
            dtid
        );
        *disposition = RemovalDisposition::ReplaceThenAdmit;
        assert!(
            self.pending_run_queue_admissions
                .insert(dtid, intent)
                .is_none(),
            "exec replacement {:?} already had a pending admission",
            dtid
        );
    }

    /// Resolve an [`AdmitIntent`] to a concrete [`AdmitSide`], consuming the
    /// post-fork PRNG draw for `RunsPostFork::Random`.
    ///
    /// Called at the drain rather than in the handler, so the draw is never
    /// consumed in host *RPC arrival* order, and the draws taken within one
    /// drain follow canonical `DetTid` order. This function only canonicalizes
    /// draws within a fixed drain; each admission site must separately bind its
    /// drain membership to deterministic scheduler state. See
    /// [`Scheduler::admit_to_run_queue`] for the causal and explicit barriers
    /// that provide that binding for all current production sites.
    fn resolve_admit_intent(&mut self, intent: AdmitIntent) -> AdmitSide {
        match intent {
            AdmitIntent::Fixed(side) => side,
            AdmitIntent::PostFork(mode) => {
                if self.child_runs_first_post_fork(mode) {
                    AdmitSide::Front
                } else {
                    AdmitSide::Back
                }
            }
        }
    }

    /// Record an intent to remove `dtid` from the run queue, applied by the
    /// daemon at the next deterministic drain point ([`step2`](Self::step2_process_blocked)).
    ///
    /// The mirror of [`Scheduler::admit_to_run_queue`] for the removal side, and
    /// deferred for the same reason: a global-request handler
    /// (`reconnect_after_exec` -> `logically_kill_thread`) runs on a backend
    /// worker and may hold the lock inside the daemon's tentative-pop window,
    /// where `RunQueue::remove_tid`'s `tentative_selection.is_none()` assert
    /// would trip and poison the scheduler mutex. Recording the removal and
    /// applying it at `step2` (window closed, guard holds) avoids that. The
    /// caller has already made the thread logically dead (cleared `next_turns`,
    /// resolved its request to `ThreadExited`), so leaving its stale run-queue
    /// entry in place until the drain is inert: the daemon skips it for any
    /// intervening turn (`step3`'s pick is validated against `next_turns`) and
    /// `are_all_quiesced` filters it out. On ptrace the drain runs at the next
    /// `step2`, before that turn's `step3` selection, so the removal is
    /// observationally immediate — the dead thread is never selected.
    fn deschedule_or_defer(&mut self, dtid: DetTid) {
        // A later logical death of a not-yet-drained exec replacement must
        // override `ReplaceThenAdmit`: `remove_blocking_entries` clears its
        // admission and this `Retire` disposition prevents resurrection.
        self.pending_run_queue_removals
            .insert(dtid, RemovalDisposition::Retire);
    }

    /// Drain removals deferred by [`Scheduler::deschedule_or_defer`] at the same
    /// deterministic `step2` point as admissions, and *before* them, so a thread
    /// killed while an admission was still buffered is not re-enqueued. The
    /// window is closed here (`tentative_selection` is `None`), so
    /// `remove_tid`'s guard holds. The `BTreeMap` makes removal order canonical;
    /// each disposition determines whether a same-raw-TID admission is stale or
    /// is the explicitly paired exec replacement.
    fn drain_pending_run_queue_removals(&mut self) {
        if self.pending_run_queue_removals.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_run_queue_removals);
        for (dtid, disposition) in pending {
            match disposition {
                RemovalDisposition::Retire => {
                    // Ordinary logical death cancels a buffered admission for
                    // the same thread incarnation.
                    self.pending_run_queue_admissions.remove(&dtid);
                }
                RemovalDisposition::ReplaceThenAdmit => {
                    assert!(
                        self.next_turns.contains_key(&dtid),
                        "exec replacement {:?} lost its scheduler registration",
                        dtid
                    );
                    assert!(
                        self.pending_run_queue_admissions.contains_key(&dtid),
                        "exec replacement {:?} lost its paired admission",
                        dtid
                    );
                    assert!(
                        !self.thread_is_logically_killed(dtid),
                        "logically dead exec replacement {:?} reached the drain",
                        dtid
                    );
                }
            }
            // Always remove the old physical queue slot before a replacement
            // admission is applied. This prevents the new image from inheriting
            // the destroyed leader's round-robin position.
            let _ = self.run_queue.remove_tid(dtid);
        }
    }

    /// Push `dtid` onto the run queue immediately, idempotently: a thread
    /// already queued is left in place rather than enqueued twice.
    fn admit_now(&mut self, dtid: DetTid, side: AdmitSide) {
        if self.run_queue.contains_tid(dtid) {
            return;
        }
        match side {
            AdmitSide::Front => {
                let _ = self.runqueue_push_front(dtid);
            }
            AdmitSide::Back => {
                let _ = self.runqueue_push_back(dtid);
            }
        }
    }

    /// Drain admissions deferred by [`Scheduler::admit_to_run_queue`] into the
    /// run queue at a single deterministic point: the very start of `step2`,
    /// before `step3_peek` opens a tentative window (so `tentative_selection` is
    /// guaranteed `None` here). Draining a `BTreeMap` visits `DetTid`s in sorted
    /// order, so the resulting run-queue state is a pure function of the
    /// deterministic schedule rather than of RPC/lock-acquisition timing.
    fn drain_pending_run_queue_admissions(&mut self) {
        if self.pending_run_queue_admissions.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_run_queue_admissions);
        // `pending` is a `BTreeMap`, so iteration visits `DetTid`s in sorted
        // order. Resolving each intent here (rather than at the racing handler)
        // means any `RunsPostFork::Random` PRNG draw is consumed in this
        // canonical order, so both the admission order *and* the chosen side are
        // pure functions of deterministic state.
        for (dtid, intent) in pending {
            // A thread retired (exec/kill) between record and drain is skipped.
            // `remove_blocking_entries` also clears the buffer on teardown, so
            // this guard is defensive against any teardown path that does not.
            if !self.next_turns.contains_key(&dtid) {
                trace!(
                    "[step2] skipping deferred admission of retired thread {:?}",
                    dtid
                );
                continue;
            }
            let side = self.resolve_admit_intent(intent);
            self.admit_now(dtid, side);
        }
    }

    /// Decide which side gets the first post-fork turn for an ordinary clone.
    pub(crate) fn child_runs_first_post_fork(&mut self, mode: RunsPostFork) -> bool {
        match mode {
            RunsPostFork::Child => true,
            RunsPostFork::Parent => false,
            RunsPostFork::Random => self.post_fork_prng.random(),
        }
    }

    /// Check if a thread is alive, but removed from run queue.
    fn thread_status(&self, dtid: DetTid) -> ThreadStatus {
        if self.run_queue.contains_tid(dtid) {
            ThreadStatus::Running
        } else {
            // Check all the places a blocked thread could be hiding.
            // TODO: this O(N) search could be made more efficient with more indexing structures.
            for v in self.blocked.futex_waiters.values() {
                for waiter in v {
                    if waiter.dettid == dtid {
                        return ThreadStatus::NotRunning;
                    }
                }
            }
            for (_, evt) in self.blocked.timed_waiters.iter() {
                match evt {
                    TimedEvent::ThreadEvt(dt) => {
                        if dt == dtid {
                            return ThreadStatus::NotRunning;
                        }
                    }
                    TimedEvent::SignalEvt(_, _, _) => {}
                }
            }
            if self.blocked.external_io_blockers.contains_key(&dtid) {
                return ThreadStatus::NotRunning;
            }
            ThreadStatus::Gone
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#252): Confirm completed and final partial slices belong in one distribution.
    /// Fold an exiting thread's completed-timeslice distribution into the
    /// scheduler's per-thread record, to be reported in the final run summary.
    pub fn record_timeslice_stats(&mut self, dettid: DetTid, stats: TimesliceStats) {
        if stats.is_empty() {
            return;
        }
        self.per_thread_timeslice
            .entry(dettid)
            .or_default()
            .merge(&stats);
    }

    /// Summarize the run after completion, as a RunSummary. This is partial because the Scheduler
    /// does not have all the necessary information.
    ///
    /// Side Effects: This also flushes the in-memory PreemptionWriter to disk.
    pub fn generate_partial_run_summary(
        &mut self,
        preemptions_to: Option<&PathBuf>,
    ) -> anyhow::Result<RunSummary> {
        let schedevent_replayed = self
            .replayer
            .as_ref()
            .map(|r| r.events_popped)
            .unwrap_or_default();
        let total_desync_stats = self
            .replayer
            .as_ref()
            .map(|r| {
                r.desync_counts
                    .values()
                    .fold(Default::default(), |x: DesyncStats, y: &DesyncStats| x + *y)
            })
            .unwrap_or_default();

        let total_desyncs = total_desync_stats.soft + total_desync_stats.hard;
        let desync_descrip = if total_desyncs > 0 {
            let mut buf = String::new();
            write!(
                buf,
                "  Encountered {} soft desyncs, {} hard (with {} at context switch points), {} resyncs ({}/{} insertion/deletion).\n  Per thread (soft,hard,@switch,resync): ",
                total_desync_stats.soft,
                total_desync_stats.hard,
                total_desync_stats.at_context_switch,
                total_desync_stats.resync_insertions + total_desync_stats.resync_deletions,
                total_desync_stats.resync_insertions,
                total_desync_stats.resync_deletions,
            )?;
            if let Some(ref replayer) = self.replayer {
                for (tid, desync_stats) in replayer.desync_counts.iter() {
                    write!(
                        buf,
                        "{}=>({},{},{},{}) ",
                        tid,
                        desync_stats.soft,
                        desync_stats.hard,
                        desync_stats.at_context_switch,
                        desync_stats.resync_insertions + desync_stats.resync_deletions,
                    )?;
                }
            }
            writeln!(buf)?;
            Some(buf)
        } else {
            None
        };

        let reprio_descrip = if let Some(pw) = self.preemption_writer.take() {
            let mut buf = String::new();
            writeln!(
                buf,
                "Record of {} preemption and reprioritization events:",
                pw.len()
            )?;
            if let Some(path) = preemptions_to {
                writeln!(buf, "  (Writing to file {:?})", path)?;
                if let Err(str) = pw.flush() {
                    tracing::warn!("{}", str);
                }
            } else {
                // Recording, but not outputting to file, so this is the only (partial) record of it:
                writeln!(buf, "{}", truncated(200, pw.into_string()))?;
            }
            Some(buf)
        } else {
            None
        };

        let num_processes = self.thread_tree.thread_group_leaders.len() as u64;
        let num_threads = self.thread_tree.size() as u64;
        let threads_descrip = format!("{}", self.thread_tree);

        // Aggregate the per-thread timeslice distributions collected at thread
        // exit. BTreeMap gives a deterministic (dettid-sorted) ordering.
        let per_thread_timeslice: Vec<(DetTid, TimesliceStats)> = self
            .per_thread_timeslice
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        let mut timeslice_stats = TimesliceStats::default();
        for (_, st) in &per_thread_timeslice {
            timeslice_stats.merge(st);
        }

        Ok(RunSummary {
            sched_turns: self.turn,
            schedevent_replayed,
            schedevent_recorded: self.recorded_event_count,
            schedevent_desynced: total_desyncs,
            // schedevent_desynced_at_context_switch: total_desyncs.at_context_switch,
            desync_descrip,
            reprio_descrip,
            threads_descrip,
            num_processes,
            num_threads,
            virttime_elapsed: 0, // Cannot fill.
            virttime_final: 0,   // Cannot fill.
            realtime_elapsed: None,
            timeslice_stats,
            per_thread_timeslice,
        })
    }

    /// Summarize the state of the scheduler while executing (verbose).
    pub fn full_summary(&self) -> String {
        let mut buf = String::new();
        write!(&mut buf, "  {}", self.run_queue).unwrap();

        let total_futex_blocked: usize = self.blocked.futex_waiters.iter().map(|v| v.1.len()).sum();
        writeln!(
            &mut buf,
            "\n  Futex-waiters, {} blocked on {} futexes:",
            total_futex_blocked,
            self.blocked.futex_waiters.len()
        )
        .unwrap();
        for x in self.blocked.futex_waiters.iter() {
            writeln!(&mut buf, "    {:?}", x).unwrap();
        }

        writeln!(
            &mut buf,
            "\n  Timed-waiters, {}:",
            self.blocked.timed_waiters.len()
        )
        .unwrap();
        for (time, dtid) in self.blocked.timed_waiters.iter() {
            writeln!(&mut buf, "    {} => {}", time, dtid).unwrap();
        }

        writeln!(
            &mut buf,
            "\n  External-IO-blocked, {}:",
            self.blocked.external_io_blockers.len(),
        )
        .unwrap();
        for x in &self.blocked.external_io_blockers {
            writeln!(&mut buf, "    {:?}", x).unwrap();
        }

        writeln!(&mut buf, "\n  Next_turns: ").unwrap();
        for (dtid, nxt) in self.next_turns.iter() {
            writeln!(
                &mut buf,
                " ==> dtid {}, req {}, resp {}",
                dtid, nxt.req, nxt.resp
            )
            .unwrap();
        }
        buf
    }

    // Return whether we should print the stacktrace after recording this event.
    // This is redundant with the consume_schedevent logic but allows us to print on either
    // recording or replay.
    pub fn record_event(&mut self, ev: &SchedEvent) -> MaybePrintStack {
        debug!(
            "[dtid {}] Record scheduled event #{}: {:?}",
            &ev.dettid, self.recorded_event_count, ev
        );
        let pw = self
            .preemption_writer
            .as_mut()
            .expect("trace_schedevent should be called only when preemption_writer is set");
        pw.insert_schedevent(ev.clone());

        let print_stack = self.try_pop_stacktrace_event(self.recorded_event_count, ev);
        self.recorded_event_count += 1;
        print_stack
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#663)
    // TODO-HUMAN-REVIEW(#869)
    // Returns the logical duration until any previously scheduled alarm, if any (zero otherwise).
    pub fn register_alarm(
        &mut self,
        detpid: DetPid,
        dettid: DetTid,
        now: LogicalTime,
        duration: LogicalTime,
        interval: LogicalTime,
        sig: Signal,
    ) -> (LogicalTime, LogicalTime) {
        let old = if duration == LogicalTime::ZERO {
            // Alarm of 0 cancels any pending signal.
            self.blocked.timed_waiters.remove_alarm(detpid)
        } else {
            let target_time = now + duration;
            self.blocked
                .timed_waiters
                .insert_alarm(target_time, detpid, dettid, sig, interval)
        };
        if let Some((old_target_time, old_interval)) = old {
            let remain_ns: u64 = old_target_time.as_nanos().saturating_sub(now.as_nanos());
            (LogicalTime::from_nanos(remain_ns), old_interval)
        } else {
            // Return 0 if no previous alarm, as per https://man7.org/linux/man-pages/man2/alarm.2.html
            (LogicalTime::ZERO, LogicalTime::ZERO)
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#869)
    pub fn register_posix_timer(
        &mut self,
        detpid: DetPid,
        dettid: DetTid,
        timer_id: i32,
        deadline: Option<LogicalTime>,
        interval: LogicalTime,
        sig: Signal,
    ) {
        if let Some(deadline) = deadline {
            self.blocked
                .timed_waiters
                .insert_posix_timer(deadline, detpid, dettid, timer_id, sig, interval);
        } else {
            self.blocked
                .timed_waiters
                .remove_posix_timer(detpid, timer_id);
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-841): Review logical ITIMER_REAL state queries.
    pub fn alarm_remaining(&self, detpid: DetPid, now: LogicalTime) -> LogicalTime {
        self.blocked
            .timed_waiters
            .alarm_time(detpid)
            .map(|deadline| {
                LogicalTime::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
            })
            .unwrap_or(LogicalTime::ZERO)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn futex_waiter(dettid: i32, bitset: u32) -> FutexWaiter {
        FutexWaiter {
            dettid: DetTid::from_raw(dettid),
            response: Ivar::new(),
            bitset,
        }
    }

    #[test]
    fn chaos_pick_is_seed_deterministic_and_covers_all_choices() {
        let choices = [10, 20, 30, 40];

        // Same seed => identical sequence of picks (the reproducibility that
        // targeted chaos relies on).
        let mut a = Pcg64Mcg::seed_from_u64(1234);
        let mut b = Pcg64Mcg::seed_from_u64(1234);
        for _ in 0..64 {
            assert_eq!(chaos_pick(&mut a, &choices), chaos_pick(&mut b, &choices));
        }

        // Over enough draws every choice is reachable (the bias actually explores
        // the space rather than pinning one option).
        let mut prng = Pcg64Mcg::seed_from_u64(9);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            seen.insert(chaos_pick(&mut prng, &choices).unwrap());
        }
        assert_eq!(
            seen,
            choices.iter().copied().collect(),
            "chaos_pick should be able to return every choice"
        );

        // An empty slice yields None rather than panicking.
        assert_eq!(chaos_pick(&mut prng, &[] as &[i32]), None);

        // The front/back coin flip used by force_unblock_thread reaches both.
        let mut prng = Pcg64Mcg::seed_from_u64(7);
        let mut both = std::collections::BTreeSet::new();
        for _ in 0..64 {
            both.insert(chaos_pick(&mut prng, &[true, false]).unwrap());
        }
        assert_eq!(both, [false, true].into_iter().collect());
    }

    #[test]
    fn post_fork_modes_are_selectable_and_random_is_seed_deterministic() {
        let config = Config {
            sched_seed: Some(1234),
            ..Default::default()
        };
        let mut fixed = Scheduler::new(&config);
        assert!(fixed.child_runs_first_post_fork(RunsPostFork::Child));
        assert!(!fixed.child_runs_first_post_fork(RunsPostFork::Parent));

        let mut first = Scheduler::new(&config);
        let mut second = Scheduler::new(&config);
        let first_sequence = (0..64)
            .map(|_| first.child_runs_first_post_fork(RunsPostFork::Random))
            .collect::<Vec<_>>();
        let second_sequence = (0..64)
            .map(|_| second.child_runs_first_post_fork(RunsPostFork::Random))
            .collect::<Vec<_>>();

        assert_eq!(first_sequence, second_sequence);
        assert!(first_sequence.contains(&true));
        assert!(first_sequence.contains(&false));
    }

    /// Register `tid` as a known, prioritized thread with an (empty) pending
    /// request, without enqueuing it. Mirrors the state a global-request handler
    /// leaves behind for a freshly created child.
    #[cfg(test)]
    fn register_known_thread(sched: &mut Scheduler, tid: DetTid) {
        sched.priorities.insert(tid, DEFAULT_PRIORITY);
        sched.next_turns.insert(
            tid,
            ThreadNextTurn {
                dettid: tid,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
            },
        );
    }

    fn install_runnable_exec_group(
        sched: &mut Scheduler,
        leader: DetTid,
        caller: DetTid,
    ) -> (DetPid, MmId, Ivar<SchedRequest>) {
        let detpid = DetPid::from_raw(leader.as_raw());
        let pre_exec_mm = MmId::initial(detpid);
        sched.thread_tree.add_child(leader, leader, true);
        sched.thread_tree.add_child(leader, caller, false);
        register_known_thread(sched, leader);
        register_known_thread(sched, caller);
        sched.runqueue_push_back(leader);
        sched.runqueue_push_back(caller);
        let old_leader_request = sched.next_turns.get(&leader).unwrap().req.clone();
        (detpid, pre_exec_mm, old_leader_request)
    }

    fn reconnect_nonleader_exec(
        sched: &mut Scheduler,
        leader: DetTid,
        caller: DetTid,
        detpid: DetPid,
        pre_exec_mm: MmId,
    ) -> Vec<DetTid> {
        sched.reconnect_after_exec(ExecReconnect {
            caller,
            new_leader: leader,
            detpid,
            pre_exec_mm,
            post_exec_mm: pre_exec_mm.for_exec(detpid),
            child_tid_addr: 0,
            reconnect_priority: Some(DEFAULT_PRIORITY),
        })
    }

    /// F1/F2: an admission deferred while a tentative_pop window is live must
    /// resolve its side -- including the `RunsPostFork::Random` PRNG draw -- at
    /// the `DetTid`-ordered drain, so the drained run queue is a pure function of
    /// deterministic state, independent of the order in which racing handlers
    /// buffered the admissions.
    #[test]
    fn deferred_admission_side_is_arrival_order_independent() {
        let config = Config {
            sched_seed: Some(0xABCD),
            runs_post_fork: RunsPostFork::Random,
            ..Default::default()
        };
        let lower = DetTid::from_raw(21);
        let higher = DetTid::from_raw(23);

        // Buffer the two children in `order`, optionally while a tentative
        // window is live, then drain. Return the drained relative order of the
        // two children (the anchor is filtered out because it is consumed by the
        // window in the `open_window` case but not otherwise -- what must be
        // deterministic is the children's order and chosen sides).
        let drained_order = |order: [DetTid; 2], open_window: bool| -> Vec<DetTid> {
            let mut sched = Scheduler::new(&config);
            let anchor = DetTid::from_raw(5);
            register_known_thread(&mut sched, anchor);
            sched.runqueue_push_back(anchor);
            register_known_thread(&mut sched, lower);
            register_known_thread(&mut sched, higher);

            if open_window {
                assert_eq!(sched.run_queue.tentative_pop_next(), Some(anchor));
                assert!(sched.run_queue.tentative_pop_in_progress());
            }
            for tid in order {
                sched.admit_to_run_queue(tid, AdmitIntent::PostFork(RunsPostFork::Random));
            }
            // Admission ALWAYS defers -- window or not -- so nothing is pushed or
            // resolved (no side chosen, no PRNG drawn) until the drain.
            assert!(sched.pending_run_queue_admissions.contains_key(&lower));
            assert!(sched.pending_run_queue_admissions.contains_key(&higher));
            assert!(!sched.run_queue.contains_tid(lower));
            assert!(!sched.run_queue.contains_tid(higher));

            if open_window {
                let _ = sched.run_queue.commit_tentative_pop();
            }
            sched.drain_pending_run_queue_admissions();
            sched
                .run_queue
                .tids()
                .copied()
                .filter(|t| *t == lower || *t == higher)
                .collect()
        };

        // The drained order/side is independent of BOTH host-timing inputs the
        // old immediate path was sensitive to: the arrival (buffering) order of
        // the racing handlers, and whether a tentative window happened to be
        // live when each handler ran.
        let baseline = drained_order([lower, higher], true);
        assert_eq!(
            baseline,
            drained_order([higher, lower], true),
            "arrival order"
        );
        assert_eq!(
            baseline,
            drained_order([lower, higher], false),
            "window state"
        );
        assert_eq!(baseline, drained_order([higher, lower], false), "both");
        assert!(baseline.contains(&lower) && baseline.contains(&higher));
    }

    /// F3: a run-queue removal requested while a tentative_pop window is live
    /// (an asynchronous exec reconnect racing the daemon) must be deferred, not
    /// applied through `remove_tid`'s `tentative_selection.is_none()` guard, and
    /// a pending-removal thread whose `next_turns` entry is already gone must not
    /// crash `are_all_quiesced`. The removal lands at the next drain.
    #[test]
    fn deferred_removal_survives_tentative_window_and_drains() {
        let mut sched = Scheduler::new(&Config::default());
        let anchor = DetTid::from_raw(5);
        let victim = DetTid::from_raw(9);
        register_known_thread(&mut sched, anchor);
        register_known_thread(&mut sched, victim);
        sched.runqueue_push_back(anchor);
        sched.runqueue_push_back(victim);

        // Daemon peeks the anchor and releases the lock (window open).
        assert_eq!(sched.run_queue.tentative_pop_next(), Some(anchor));
        assert!(sched.run_queue.tentative_pop_in_progress());

        // A racing handler descheduling the victim must buffer, not panic.
        sched.deschedule_or_defer(victim);
        assert!(sched.pending_run_queue_removals.contains_key(&victim));
        assert!(
            sched.run_queue.contains_tid(victim),
            "removal is deferred, so the stale entry lingers until the drain"
        );

        // The handler has also made the victim logically dead: its next_turns
        // entry is gone. Iterating quiescence must SKIP the victim (filtered by
        // pending_run_queue_removals) rather than panic in check_request on the
        // missing next_turns entry -- without the filter this call panics.
        sched.next_turns.remove(&victim);
        let _ = sched.are_all_quiesced();

        // Close the window and drain: the victim is gone, the anchor remains.
        let _ = sched.run_queue.commit_tentative_pop();
        sched.drain_pending_run_queue_removals();
        assert!(sched.pending_run_queue_removals.is_empty());
        assert!(!sched.run_queue.contains_tid(victim));
    }

    /// Always-defer invariant (codex finding 1/2): even with NO tentative window
    /// live, a global-request handler NEVER pushes or pops the run queue
    /// directly -- both admission and removal are buffered and take effect only
    /// at the deterministic step2 drain. This removes the host-timing-dependent
    /// immediate path: whichever daemon phase a handler happened to race, it only
    /// records intent, so the run-queue mutation is applied at one fixed point in
    /// DetTid order regardless of host arrival timing.
    #[test]
    fn run_queue_mutations_always_defer_to_the_drain() {
        let mut sched = Scheduler::new(&Config::default());
        let keep = DetTid::from_raw(7);
        let victim = DetTid::from_raw(9);
        register_known_thread(&mut sched, keep);
        register_known_thread(&mut sched, victim);
        sched.runqueue_push_back(victim); // already queued; to be removed
        assert!(!sched.run_queue.tentative_pop_in_progress());

        // No window live, yet both mutations buffer rather than apply.
        sched.admit_to_run_queue(keep, AdmitIntent::Fixed(AdmitSide::Back));
        sched.deschedule_or_defer(victim);
        assert!(sched.pending_run_queue_admissions.contains_key(&keep));
        assert!(sched.pending_run_queue_removals.contains_key(&victim));
        assert!(!sched.run_queue.contains_tid(keep), "admission deferred");
        assert!(sched.run_queue.contains_tid(victim), "removal deferred");

        // The daemon applies them at the drain (removals first, then admissions).
        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();
        assert!(sched.run_queue.contains_tid(keep));
        assert!(!sched.run_queue.contains_tid(victim));
        assert!(sched.pending_run_queue_admissions.is_empty());
        assert!(sched.pending_run_queue_removals.is_empty());
    }

    /// A thread admitted and then killed before the drain must end up neither
    /// queued nor pending: removals drain first and the retired-thread skip in
    /// the admission drain drops the buffered admission for a thread with no
    /// next_turns entry.
    #[test]
    fn buffered_admission_cancelled_by_buffered_removal() {
        let mut sched = Scheduler::new(&Config::default());
        let tid = DetTid::from_raw(11);
        register_known_thread(&mut sched, tid);

        sched.admit_to_run_queue(tid, AdmitIntent::Fixed(AdmitSide::Back));
        sched.deschedule_or_defer(tid);

        // The thread is retired before the drain: its next_turns entry is gone.
        sched.next_turns.remove(&tid);
        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();

        assert!(!sched.run_queue.contains_tid(tid));
        assert!(sched.pending_run_queue_admissions.is_empty());
        assert!(sched.pending_run_queue_removals.is_empty());
    }

    /// A successful nonleader exec retires the old process leader and reuses
    /// its raw TID for the caller's replacement image.  The old-leader removal
    /// and replacement-leader admission therefore share a `DetTid`, but they do
    /// not name the same thread incarnation: the drain must remove the former
    /// without cancelling the latter.
    #[test]
    fn nonleader_exec_removal_preserves_replacement_admission() {
        let config = Config {
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let mut sched = Scheduler::new(&config);
        let leader = DetTid::from_raw(17);
        let caller = DetTid::from_raw(18);
        let detpid = DetPid::from_raw(leader.as_raw());
        let pre_exec_mm = MmId::initial(detpid);

        sched.thread_tree.add_child(leader, leader, true);
        sched.thread_tree.add_child(leader, caller, false);
        register_known_thread(&mut sched, leader);
        register_known_thread(&mut sched, caller);
        sched.runqueue_push_back(leader);
        sched.runqueue_push_back(caller);

        let old_leader_request = sched.next_turns.get(&leader).unwrap().req.clone();
        let retired = sched.reconnect_after_exec(ExecReconnect {
            caller,
            new_leader: leader,
            detpid,
            pre_exec_mm,
            post_exec_mm: pre_exec_mm.for_exec(detpid),
            child_tid_addr: 0,
            reconnect_priority: Some(DEFAULT_PRIORITY),
        });

        assert_eq!(retired, vec![leader, caller]);
        assert!(matches!(old_leader_request.try_read(), Some(Err(_))));
        assert!(sched.pending_run_queue_removals.contains_key(&leader));
        assert!(sched.pending_run_queue_admissions.contains_key(&leader));

        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();

        assert_eq!(
            sched
                .run_queue
                .tids()
                .filter(|dettid| **dettid == leader)
                .count(),
            1,
            "the first eligible drain must contain exactly one replacement leader"
        );
        assert!(!sched.run_queue.contains_tid(caller));
        assert!(sched.next_turns.contains_key(&leader));
        assert!(sched.pending_run_queue_admissions.is_empty());
        assert!(sched.pending_run_queue_removals.is_empty());
    }

    #[test]
    fn exec_replacement_killed_before_drain_is_not_resurrected() {
        let config = Config {
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let mut sched = Scheduler::new(&config);
        let leader = DetTid::from_raw(17);
        let caller = DetTid::from_raw(18);
        let (detpid, pre_exec_mm, _) = install_runnable_exec_group(&mut sched, leader, caller);

        reconnect_nonleader_exec(&mut sched, leader, caller, detpid, pre_exec_mm);
        assert_eq!(
            sched.pending_run_queue_removals.get(&leader),
            Some(&RemovalDisposition::ReplaceThenAdmit)
        );

        sched.logically_kill_thread(&leader, &detpid, pre_exec_mm.for_exec(detpid));
        assert_eq!(
            sched.pending_run_queue_removals.get(&leader),
            Some(&RemovalDisposition::Retire)
        );
        assert!(!sched.pending_run_queue_admissions.contains_key(&leader));

        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();

        assert!(!sched.run_queue.contains_tid(leader));
        assert!(!sched.next_turns.contains_key(&leader));
        assert!(sched.pending_run_queue_admissions.is_empty());
        assert!(sched.pending_run_queue_removals.is_empty());
    }

    #[test]
    fn exec_reconnect_only_buffers_while_tentative_selection_is_live() {
        let config = Config {
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let mut sched = Scheduler::new(&config);
        let anchor = DetTid::from_raw(3);
        let leader = DetTid::from_raw(17);
        let caller = DetTid::from_raw(18);
        register_known_thread(&mut sched, anchor);
        sched.runqueue_push_back(anchor);
        let (detpid, pre_exec_mm, _) = install_runnable_exec_group(&mut sched, leader, caller);

        assert_eq!(sched.run_queue.tentative_pop_next(), Some(anchor));
        let queue_during_window = sched.run_queue.tids().copied().collect::<Vec<_>>();

        reconnect_nonleader_exec(&mut sched, leader, caller, detpid, pre_exec_mm);

        assert!(sched.run_queue.tentative_pop_in_progress());
        assert_eq!(
            sched.run_queue.tids().copied().collect::<Vec<_>>(),
            queue_during_window,
            "the reconnect handler must not mutate a tentatively selected queue"
        );
        assert_eq!(
            sched.pending_run_queue_removals.get(&leader),
            Some(&RemovalDisposition::ReplaceThenAdmit)
        );

        sched.run_queue.undo_tentative_pop();
        sched.drain_pending_run_queue_removals();
        sched.drain_pending_run_queue_admissions();

        assert_eq!(
            sched
                .run_queue
                .tids()
                .filter(|dettid| **dettid == leader)
                .count(),
            1
        );
        assert!(sched.run_queue.contains_tid(anchor));
        assert!(!sched.run_queue.contains_tid(caller));
    }

    #[derive(Debug, Clone, Copy)]
    enum ExecReconnectTiming {
        BeforeDaemon,
        AfterCallerWait { yields: usize },
        CallerResolvedBeforeReconnect,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct ExecDrainObservation {
        queue: Vec<DetTid>,
        next_post_fork_draw: bool,
        turn: u64,
        old_leader_registration_survived: bool,
    }

    async fn observe_exec_reconnect_drain(timing: ExecReconnectTiming) -> ExecDrainObservation {
        let config = Config {
            sched_seed: Some(0x5107),
            runs_post_fork: RunsPostFork::Random,
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let sched = Arc::new(Mutex::new(Scheduler::new(&config)));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&config)));
        let leader = DetTid::from_raw(17);
        let caller = DetTid::from_raw(18);
        let lower = DetTid::from_raw(31);
        let higher = DetTid::from_raw(37);
        let barrier_parent = DetTid::from_raw(99);
        let detpid = DetPid::from_raw(leader.as_raw());
        let pre_exec_mm = MmId::initial(detpid);

        let (caller_request, old_leader_request) = {
            let mut s = sched.lock().unwrap();
            s.thread_tree.add_child(leader, leader, true);
            s.thread_tree.add_child(leader, caller, false);
            register_known_thread(&mut s, leader);
            register_known_thread(&mut s, caller);
            // Give the destroyed leader a visibly different queue band. The
            // replacement must use the caller's priority at the ordinary tail.
            s.priorities.insert(leader, runqueue::FIRST_PRIORITY);
            s.runqueue_push_back(leader);
            s.runqueue_push_back(caller);
            s.next_turns
                .get(&leader)
                .unwrap()
                .req
                .put(Ok(Resources::new(leader)));

            register_known_thread(&mut s, lower);
            register_known_thread(&mut s, higher);
            s.admit_to_run_queue(lower, AdmitIntent::PostFork(RunsPostFork::Random));
            s.admit_to_run_queue(higher, AdmitIntent::PostFork(RunsPostFork::Random));
            // `step2` drains first, then this unresolved barrier returns
            // `SkipTurn`, exposing the exact first-drain queue before step3 can
            // tentatively select or rotate it.
            s.vfork_barriers.insert(barrier_parent, None);
            (
                s.next_turns.get(&caller).unwrap().req.clone(),
                s.next_turns.get(&leader).unwrap().req.clone(),
            )
        };

        if matches!(timing, ExecReconnectTiming::BeforeDaemon) {
            reconnect_nonleader_exec(
                &mut sched.lock().unwrap(),
                leader,
                caller,
                detpid,
                pre_exec_mm,
            );
        } else if matches!(timing, ExecReconnectTiming::CallerResolvedBeforeReconnect) {
            caller_request.put(Ok(Resources::new(caller)));
        }

        let turn_sched = sched.clone();
        let turn_time = global_time.clone();
        let turn = tokio::spawn(async move {
            let last: Result<Resources, SkipTurn> = Err(SkipTurn);
            do_a_turn_blocking(turn_sched, turn_time, &last).await
        });

        if let ExecReconnectTiming::AfterCallerWait { yields } = timing {
            let mut saw_waiter = false;
            for _ in 0..1_000 {
                if caller_request.to_string() == "<ivar HasWaiter>" {
                    saw_waiter = true;
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(saw_waiter, "daemon never waited on the exec caller request");
            for _ in 0..yields {
                tokio::task::yield_now().await;
            }
            reconnect_nonleader_exec(
                &mut sched.lock().unwrap(),
                leader,
                caller,
                detpid,
                pre_exec_mm,
            );
        }

        assert!(turn.await.expect("scheduler task panicked").is_err());
        let mut s = sched.lock().unwrap();
        let observation = ExecDrainObservation {
            queue: s.run_queue.tids().copied().collect(),
            next_post_fork_draw: s.child_runs_first_post_fork(RunsPostFork::Random),
            turn: s.turn,
            old_leader_registration_survived: s
                .next_turns
                .get(&leader)
                .is_some_and(|turn| turn.req == old_leader_request),
        };
        if !matches!(timing, ExecReconnectTiming::CallerResolvedBeforeReconnect) {
            assert_eq!(
                observation
                    .queue
                    .iter()
                    .filter(|tid| **tid == leader)
                    .count(),
                1
            );
            assert!(!observation.queue.contains(&caller));
            assert!(!observation.old_leader_registration_survived);
            assert!(s.pending_run_queue_admissions.is_empty());
            assert!(s.pending_run_queue_removals.is_empty());
        }
        observation
    }

    #[tokio::test]
    async fn exec_reconnect_caller_gate_fixes_first_drain_membership_and_prng() {
        let canonical = observe_exec_reconnect_drain(ExecReconnectTiming::BeforeDaemon).await;
        for yields in [0, 1, 64] {
            assert_eq!(
                canonical,
                observe_exec_reconnect_drain(ExecReconnectTiming::AfterCallerWait { yields }).await,
                "host delay of {yields} yields changed the first eligible drain"
            );
        }

        let broken =
            observe_exec_reconnect_drain(ExecReconnectTiming::CallerResolvedBeforeReconnect).await;
        assert!(broken.old_leader_registration_survived);
        assert_ne!(
            canonical.queue, broken.queue,
            "the deliberate caller-gate violation was inert"
        );
    }

    /// F6 (real-path regression): when the thread `step3_peek` tentatively
    /// selected dies while the daemon awaits its request, `do_a_turn_blocking`
    /// takes the `Err(ThreadExited)` fizzle arm. That arm MUST undo the tentative
    /// pop so the selection does not outlive the turn; otherwise the next pass's
    /// step2 removal drain calls `remove_tid` while `tentative_selection` is
    /// still `Some`, tripping the run queue's transaction guard -- the "reconnect
    /// panic moved one pass" defect (reachable in NORMAL async-DBI operation, not
    /// just the reviewed edge case: any thread that exits during the await window
    /// races here). This drives the ACTUAL async daemon function end to end
    /// rather than poking `RunQueue` directly, which is the coverage gap the
    /// pre-existing tests left. Positive control: after the fizzle the window is
    /// closed and the following real step2 removal drain does not panic.
    #[tokio::test]
    async fn reconnect_fizzle_closes_window_so_next_removal_drain_is_safe() {
        let config = Config::default();
        let sched = Arc::new(Mutex::new(Scheduler::new(&config)));
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&config)));
        let dead = DetTid::from_raw(7);
        {
            let mut s = sched.lock().unwrap();
            s.priorities.insert(dead, DEFAULT_PRIORITY);
            s.next_turns.insert(
                dead,
                ThreadNextTurn {
                    dettid: dead,
                    child_tid_addr: 0,
                    // Request already resolved to `ThreadExited`: the daemon will
                    // observe the thread died the moment it awaits `req.get()`.
                    req: Ivar::full(Err(ThreadExited)),
                    resp: Ivar::new(),
                },
            );
            s.runqueue_push_back(dead);
        }
        let last: Result<Resources, SkipTurn> = Err(SkipTurn);

        // Pass 1: step1 sees a filled request (quiescent), step3 tentatively
        // pops `dead`, then `req.get().await` yields `Err(ThreadExited)`. The
        // fizzle arm returns `SkipTurn` and undoes the tentative pop.
        let first = do_a_turn_blocking(sched.clone(), global_time.clone(), &last).await;
        assert!(first.is_err(), "fizzled reconnect turn skips");
        assert!(
            !sched.lock().unwrap().run_queue.tentative_pop_in_progress(),
            "the ThreadExited arm must undo the tentative pop"
        );

        // Pass 2: the dead thread's run-queue removal is now buffered, exactly
        // as a reconnect/kill handler would leave it. Because pass 1 closed the
        // window, the real step2 removal drain calls `remove_tid` with
        // `tentative_selection == None` and does NOT trip the guard. Pre-fix
        // (window left open) this call panicked.
        {
            let mut s = sched.lock().unwrap();
            s.deschedule_or_defer(dead);
            s.next_turns.remove(&dead);
            let _ = s.step2_process_blocked(&global_time);
            assert!(!s.run_queue.contains_tid(dead), "dead thread drained out");
            assert!(s.pending_run_queue_removals.is_empty());
        }
    }

    /// F8 (non-committing fizzle), part 1: the branch itself, both ways.
    ///
    /// NEGATIVE -- the defect. A selected thread whose `next_turns` entry
    /// vanished during the daemon's post-await window must abandon the turn:
    /// report `SkipTurn` (it commits nothing -- steps 4-7 are bypassed) AND
    /// close the tentative window `step3_peek` opened. Before the fix this
    /// branch fell through to `Ok(rsrcs)`, which `bump_global_time` reads as a
    /// completed turn and answers with a virtual-time advance.
    /// POSITIVE -- not inert. A thread that is still registered proceeds: `Ok`,
    /// with its tentative selection left open for step4 to commit. Without this
    /// side an `abort_turn_if_thread_vanished` that simply always skipped would
    /// pass the negative and silently stall the scheduler.
    #[test]
    fn a_vanished_thread_aborts_its_turn_and_a_live_one_does_not() {
        let config = Config::default();
        let vanishing = DetTid::from_raw(11);
        let live = DetTid::from_raw(13);

        let mut sched = Scheduler::new(&config);
        register_known_thread(&mut sched, vanishing);
        register_known_thread(&mut sched, live);
        sched.runqueue_push_back(vanishing);
        sched.runqueue_push_back(live);

        // NEGATIVE: retire `vanishing` after its selection was tentatively
        // popped, exactly as teardown does inside the post-await window.
        assert_eq!(
            sched.run_queue.tentative_pop_tid(vanishing),
            Some(vanishing)
        );
        assert!(sched.run_queue.tentative_pop_in_progress());
        sched.next_turns.remove(&vanishing);
        assert!(
            sched.abort_turn_if_thread_vanished(vanishing).is_err(),
            "a turn that bypasses steps 4-7 must report SkipTurn, never success: \
             reporting Ok lets bump_global_time advance virtual time for a turn \
             that committed nothing"
        );
        assert!(
            !sched.run_queue.tentative_pop_in_progress(),
            "the abandoned turn must also close its tentative window"
        );

        // POSITIVE: a still-registered thread is untouched and proceeds to
        // step4 with its selection still open to commit.
        assert_eq!(sched.run_queue.tentative_pop_tid(live), Some(live));
        assert!(sched.run_queue.tentative_pop_in_progress());
        assert!(
            sched.abort_turn_if_thread_vanished(live).is_ok(),
            "a live thread's turn must proceed"
        );
        assert!(
            sched.run_queue.tentative_pop_in_progress(),
            "a proceeding turn must keep its tentative selection open for step4"
        );
    }

    /// F8 (non-committing fizzle must not advance virtual time): both sides of
    /// the consequence, stated directly against `bump_global_time` -- the code
    /// that turns a turn's returned `Result` into a virtual-time decision.
    ///
    /// The defect: the fizzle arm where `req.get()` resolved `Ok` but the
    /// thread's `next_turns` entry vanished before the daemon re-acquired the
    /// lock skips steps 4-7, commits nothing, and used to fall through to
    /// `Ok(rsrcs)` -- landing in the advancing branch below and adding a
    /// DETLOG-visible tick for work that never happened. It now reports
    /// `Err(SkipTurn)` like its `ThreadExited` sibling, so this pair of
    /// assertions is what that fix buys.
    ///
    /// Deliberately NOT an end-to-end test of that arm. Reaching it requires
    /// teardown to clear `next_turns` inside the host-scheduling gap between the
    /// await resolving and the re-lock, and that gap is not constructible from a
    /// test: `step1_check_quiescence` only proceeds once every thread's request
    /// is already filled, so `req.get()` never suspends and there is no window a
    /// test can hold open. An attempt to win the lock in that gap is a genuine
    /// race that would usually lose (and deadlocks outright on a current-thread
    /// runtime, where the daemon's blocking `sched.lock()` stalls the whole
    /// executor). F6's `reconnect_fizzle_closes_window_so_next_removal_drain_is_
    /// safe` does drive the real `do_a_turn_blocking` for the sibling arm.
    ///
    /// NEGATIVE: a skipped turn leaves virtual time exactly where it was. That is
    /// what the F8 fix buys; before it, the non-committing arm reported `Ok` and
    /// landed in the advancing branch.
    /// POSITIVE: a committed, non-internal turn *does* advance it. Without this
    /// side the negative would pass vacuously for a `bump_global_time` that had
    /// simply stopped advancing time at all.
    #[test]
    fn virtual_time_advances_for_a_committed_turn_and_not_for_a_skipped_one() {
        let config = Config::default();
        let runnable = DetTid::from_raw(13);

        // A non-empty run queue keeps the "only waiting on external events"
        // guard from suppressing the advance for an unrelated reason.
        let mut advancing = Scheduler::new(&config);
        advancing.priorities.insert(runnable, DEFAULT_PRIORITY);
        advancing.runqueue_push_back(runnable);
        let advancing_time = Mutex::new(GlobalTime::new(&config));
        let before_commit = advancing_time.lock().unwrap().as_nanos();
        advancing.bump_global_time(&advancing_time, &Ok(Resources::new(runnable)));
        let after_commit = advancing_time.lock().unwrap().as_nanos();
        assert!(
            after_commit > before_commit,
            "a committed turn must advance virtual time ({before_commit:?} -> {after_commit:?})"
        );

        let mut skipping = Scheduler::new(&config);
        skipping.priorities.insert(runnable, DEFAULT_PRIORITY);
        skipping.runqueue_push_back(runnable);
        let skipping_time = Mutex::new(GlobalTime::new(&config));
        let before_skip = skipping_time.lock().unwrap().as_nanos();
        skipping.bump_global_time(&skipping_time, &Err(SkipTurn));
        let after_skip = skipping_time.lock().unwrap().as_nanos();
        assert_eq!(
            after_skip, before_skip,
            "a skipped turn must not advance virtual time"
        );
    }

    /// Liveness (negative control) for the F6 fix above: proves the guard the
    /// fizzle arm protects is real, so the positive test is not vacuous. If a
    /// fizzled turn does NOT undo its tentative pop (the pre-fix behavior), the
    /// next pass's step2 removal drain calls `remove_tid` while
    /// `tentative_selection` is `Some`, and the run queue's transaction guard
    /// panics. This is precisely the panic `undo_tentative_pop` prevents.
    #[test]
    #[should_panic(expected = "tentative_selection.is_none()")]
    fn removal_drain_panics_if_tentative_window_left_open() {
        let mut sched = Scheduler::new(&Config::default());
        let dead = DetTid::from_raw(7);
        register_known_thread(&mut sched, dead);
        sched.runqueue_push_back(dead);

        // Simulate step3_peek selecting `dead` with the turn neither committing
        // nor undoing -- i.e., the fizzle arm WITHOUT its undo.
        assert_eq!(sched.run_queue.tentative_pop_next(), Some(dead));
        assert!(sched.run_queue.tentative_pop_in_progress());

        // Next pass buffers the removal and drains: remove_tid trips the guard.
        sched.deschedule_or_defer(dead);
        sched.next_turns.remove(&dead);
        sched.drain_pending_run_queue_removals();
    }

    /// F7 (adjacent-snapshot sensitivity): this fixture directly places two
    /// admissions in adjacent step2 drains; it does not model a production
    /// handler protocol. Every current asynchronous admission site separately
    /// fixes snapshot membership: ordinary clone buffers before the parent's
    /// `ParentContinue`, `vfork` uses its registration barrier, and exec
    /// reconnect buffers before retiring and resolving the caller request. See
    /// [`Scheduler::admit_to_run_queue`] for those causal bindings.
    ///
    /// The synthetic split remains a negative/sensitivity bracket for that
    /// requirement. Replaying one fixed split at one seed is reproducible, but
    /// swapping which child occupies the first drain changes the resolved queue
    /// order. Thus a future unanchored admission site would make host-selected
    /// membership observable; this test must not be read as evidence that any
    /// current production site is unanchored.
    #[test]
    fn deferred_admission_binds_to_snapshot_membership_across_adjacent_drains() {
        let config = Config {
            sched_seed: Some(0x5107),
            runs_post_fork: RunsPostFork::Random,
            ..Default::default()
        };
        let anchor = DetTid::from_raw(3);
        let a = DetTid::from_raw(31);
        let b = DetTid::from_raw(37);

        // Admit `first` in drain 1 and `second` in drain 2 (two adjacent step2
        // drains) and return the final run-queue order relative to a fixed
        // anchor, which encodes each child's resolved front/back side.
        let split = |first: DetTid, second: DetTid| -> Vec<DetTid> {
            let mut sched = Scheduler::new(&config);
            register_known_thread(&mut sched, anchor);
            register_known_thread(&mut sched, a);
            register_known_thread(&mut sched, b);
            sched.runqueue_push_back(anchor);

            sched.admit_to_run_queue(first, AdmitIntent::PostFork(RunsPostFork::Random));
            sched.drain_pending_run_queue_admissions(); // drain 1
            sched.admit_to_run_queue(second, AdmitIntent::PostFork(RunsPostFork::Random));
            sched.drain_pending_run_queue_admissions(); // drain 2 (adjacent)

            sched.run_queue.tids().copied().collect()
        };

        // Fixed synthetic membership is deterministic across identical replays.
        let canonical = split(a, b);
        assert_eq!(
            canonical,
            split(a, b),
            "identical schedule -> identical result"
        );
        assert!(canonical.contains(&a) && canonical.contains(&b) && canonical.contains(&anchor));

        // Sensitivity control: changing synthetic snapshot membership changes
        // the outcome for this seed, so a missing production anchor would be
        // observable rather than inert.
        assert_ne!(
            split(a, b),
            split(b, a),
            "swapping snapshot membership changes the resolved order"
        );
    }

    #[test]
    fn vfork_registration_barrier_blocks_until_child_registration() {
        let mut scheduler = Scheduler::new(&Config::default());
        let parent = DetTid::from_raw(3);
        let child = DetTid::from_raw(5);
        scheduler.vfork_barriers.insert(parent, None);

        assert!(scheduler.step2a_wait_for_vfork_barrier().is_err());
        scheduler.complete_vfork_registration(parent, child);
        assert!(scheduler.step2a_wait_for_vfork_barrier().is_ok());
        assert_eq!(scheduler.vfork_barriers.get(&parent), Some(&Some(child)));
    }

    #[test]
    fn vfork_registration_barrier_releases_failed_clone() {
        let mut scheduler = Scheduler::new(&Config::default());
        let parent = DetTid::from_raw(3);
        let op_id = ExternalOpId::new(parent, 7);
        let mut continuation = Resources::new(parent);
        continuation.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);

        scheduler.vfork_barriers.insert(parent, None);
        scheduler.next_turns.insert(
            parent,
            ThreadNextTurn {
                dettid: parent,
                child_tid_addr: 0,
                req: Ivar::full(Ok(continuation)),
                resp: Ivar::new(),
            },
        );

        assert!(scheduler.step2a_wait_for_vfork_barrier().is_ok());
        assert!(scheduler.vfork_barriers.is_empty());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1152): Review deferred vfork child registration.
    #[test]
    fn vfork_registration_barrier_waits_for_deferred_child_at_continuation() {
        // On a backend that defers the child spawn (e.g. KVM), the parent posts its continuation
        // BEFORE the child registers. An unfulfilled barrier at continuation must be kept, not
        // torn down as a failed clone; otherwise the late child panics on registration.
        let config = Config {
            backend_defers_vfork_child_registration: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let parent = DetTid::from_raw(3);
        let child = DetTid::from_raw(5);
        let op_id = ExternalOpId::new(parent, 7);
        let mut continuation = Resources::new(parent);
        continuation.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);

        scheduler.vfork_barriers.insert(parent, None);
        scheduler.next_turns.insert(
            parent,
            ThreadNextTurn {
                dettid: parent,
                child_tid_addr: 0,
                req: Ivar::full(Ok(continuation)),
                resp: Ivar::new(),
            },
        );

        // Parent is at its continuation but the child has not registered: keep waiting, keep the
        // barrier (the failed-clone teardown must NOT fire on a deferring backend).
        assert!(scheduler.step2a_wait_for_vfork_barrier().is_err());
        assert_eq!(scheduler.vfork_barriers.get(&parent), Some(&None));

        // The deferred child registers; now the barrier is fulfilled and released.
        scheduler.complete_vfork_registration(parent, child);
        assert!(scheduler.step2a_wait_for_vfork_barrier().is_ok());
        assert!(scheduler.vfork_barriers.is_empty());
    }

    // TODO-HUMAN-REVIEW(PR-1152): Review failed deferred-vfork cancellation.
    #[test]
    fn vfork_registration_barrier_releases_deferred_failed_clone() {
        let config = Config {
            backend_defers_vfork_child_registration: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let parent = DetTid::from_raw(3);
        let op_id = ExternalOpId::new(parent, 7);
        let mut failure = Resources::new(parent);
        failure.insert(ResourceID::VforkFailed(op_id), Permission::RW);

        scheduler.vfork_barriers.insert(parent, None);
        scheduler.blocked.external_io_blockers.insert(parent, op_id);
        scheduler.next_turns.insert(
            parent,
            ThreadNextTurn {
                dettid: parent,
                child_tid_addr: 0,
                req: Ivar::full(Ok(failure)),
                resp: Ivar::new(),
            },
        );

        // The explicit failure proves that no deferred child is coming, so step2a cancels the
        // barrier instead of waiting forever. The ordinary external-continuation path can then
        // requeue the parent, whose syscall handler still owns and returns the original errno.
        assert!(scheduler.step2a_wait_for_vfork_barrier().is_ok());
        assert!(scheduler.vfork_barriers.is_empty());
        assert!(scheduler.step2c_process_io_blockers().is_ok());
        assert!(scheduler.blocked.external_io_blockers.is_empty());
        assert!(scheduler.run_queue.contains_tid(parent));
    }

    #[test]
    fn external_io_continuation_does_not_overtake_runnable_peer() {
        let mut scheduler = Scheduler::new(&Config::default());
        let signal_waiter = DetTid::from_raw(11);
        let exiting_child = DetTid::from_raw(17);
        let op_id = ExternalOpId::new(signal_waiter, 291);
        let mut continuation = Resources::new(signal_waiter);
        continuation.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);

        scheduler.priorities.insert(signal_waiter, DEFAULT_PRIORITY);
        scheduler.priorities.insert(exiting_child, DEFAULT_PRIORITY);
        scheduler.runqueue_push_back(exiting_child);
        scheduler
            .blocked
            .external_io_blockers
            .insert(signal_waiter, op_id);
        scheduler.next_turns.insert(
            signal_waiter,
            ThreadNextTurn {
                dettid: signal_waiter,
                child_tid_addr: 0,
                req: Ivar::full(Ok(continuation)),
                resp: Ivar::new(),
            },
        );

        assert!(scheduler.step2c_process_io_blockers().is_ok());
        assert_eq!(
            scheduler.blocked.external_io_blockers.get(&signal_waiter),
            Some(&op_id)
        );
        assert_eq!(
            scheduler.run_queue.tentative_pop_next(),
            Some(exiting_child)
        );
    }

    #[test]
    fn futex_wake_bitset_only_selects_intersecting_waiters() {
        let mut waiters = vec![
            futex_waiter(1, 0b0001),
            futex_waiter(2, 0b0010),
            futex_waiter(3, 0b0011),
        ];

        let matching = take_matching_futex_waiters(&mut waiters, 0b0010);
        assert_eq!(
            matching
                .iter()
                .map(|waiter| waiter.dettid)
                .collect::<Vec<_>>(),
            [DetTid::from_raw(2), DetTid::from_raw(3)]
        );
        assert_eq!(
            waiters
                .iter()
                .map(|waiter| waiter.dettid)
                .collect::<Vec<_>>(),
            [DetTid::from_raw(1)]
        );

        let matching = take_matching_futex_waiters(&mut waiters, 0);
        assert!(
            matching.is_empty(),
            "a zero wake bitset must match no waiter"
        );
        assert_eq!(waiters.len(), 1, "nonmatching waiters must remain queued");
    }

    #[test]
    fn test_my_thread_group1() {
        let mut tree: ThreadTree = Default::default();
        let p1 = DetPid::from_raw(100);
        let p2 = DetPid::from_raw(200);
        let p3 = DetPid::from_raw(300);
        tree.add_child(p1, p1, true);
        tree.add_child(p1, p2, false);
        tree.add_child(p1, p3, false);
        let mut v = tree.my_thread_group(&p2);
        v.sort();
        assert_eq!(&v, &[p1, p2, p3]);
        let s = format!("{}", tree);
        assert!(!s.is_empty());
    }

    #[test]
    fn test_my_thread_group2() {
        let mut tree: ThreadTree = Default::default();
        let p1 = DetPid::from_raw(100);
        let p2 = DetPid::from_raw(200);
        let p3 = DetPid::from_raw(300);
        let p4 = DetPid::from_raw(400);
        let p5 = DetPid::from_raw(500);
        tree.add_child(p1, p1, true);
        tree.add_child(p1, p2, false);
        tree.add_child(p1, p3, true); // second group leader
        tree.add_child(p3, p4, false);
        tree.add_child(p4, p5, false);
        let mut v = tree.my_thread_group(&p2);
        v.sort();
        assert_eq!(&v, &[p1, p2]);

        let mut v = tree.my_thread_group(&p5);
        v.sort();
        assert_eq!(&v, &[p3, p4, p5]);
        let s = tree.pretty_print();
        assert!(!s.is_empty());
    }

    #[test]
    fn logically_kill_thread_unblocks_pending_rpc() {
        let config = Config {
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let dettid = DetTid::from_raw(100);
        let detpid = DetPid::from_raw(100);
        let response = Ivar::new();
        scheduler.thread_tree.add_child(dettid, dettid, true);
        scheduler.next_turns.insert(
            dettid,
            ThreadNextTurn {
                dettid,
                child_tid_addr: 0,
                req: Ivar::full(Ok(Resources::new(dettid))),
                resp: response.clone(),
            },
        );

        scheduler.logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));

        assert!(matches!(
            response.try_read(),
            Some(SchedResponse::Signaled())
        ));
    }

    #[test]
    fn logically_kill_running_thread_does_not_preload_response() {
        let config = Config {
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let dettid = DetTid::from_raw(100);
        let detpid = DetPid::from_raw(100);
        let request = Ivar::new();
        let response = Ivar::new();
        scheduler.thread_tree.add_child(dettid, dettid, true);
        scheduler.next_turns.insert(
            dettid,
            ThreadNextTurn {
                dettid,
                child_tid_addr: 0,
                req: request.clone(),
                resp: response.clone(),
            },
        );

        scheduler.logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));

        assert!(matches!(request.try_read(), Some(Err(ThreadExited))));
        assert!(response.try_read().is_none());
    }

    #[test]
    fn ptrace_kill_leaves_pending_rpc_to_kernel_teardown() {
        let mut scheduler = Scheduler::new(&Config::default());
        let dettid = DetTid::from_raw(100);
        let detpid = DetPid::from_raw(100);
        let response = Ivar::new();
        scheduler.thread_tree.add_child(dettid, dettid, true);
        scheduler.next_turns.insert(
            dettid,
            ThreadNextTurn {
                dettid,
                child_tid_addr: 0,
                req: Ivar::full(Ok(Resources::new(dettid))),
                resp: response.clone(),
            },
        );

        scheduler.logically_kill_thread(&dettid, &detpid, MmId::initial(detpid));

        assert!(response.try_read().is_none());
    }

    #[test]
    fn alarm_deadline_uses_observed_logical_time() {
        let mut scheduler = Scheduler::new(&Config::default());
        let detpid = DetPid::from_raw(100);
        let dettid = DetTid::from_raw(101);
        let now = LogicalTime::from_nanos(1_000);
        let duration = LogicalTime::from_nanos(250);

        assert_eq!(
            scheduler.register_alarm(
                detpid,
                dettid,
                now,
                duration,
                LogicalTime::ZERO,
                Signal::SIGALRM,
            ),
            (LogicalTime::ZERO, LogicalTime::ZERO)
        );
        assert_eq!(
            scheduler.blocked.timed_waiters.iter().collect::<Vec<_>>(),
            vec![(
                LogicalTime::from_nanos(1_250),
                TimedEvent::SignalEvt(
                    timed_waiters::SignalTimerId::Alarm(detpid),
                    dettid,
                    Signal::SIGALRM,
                )
            )]
        );
        assert_eq!(
            scheduler.alarm_remaining(detpid, LogicalTime::from_nanos(1_100)),
            LogicalTime::from_nanos(150)
        );
        assert_eq!(
            scheduler.alarm_remaining(detpid, LogicalTime::from_nanos(1_300)),
            LogicalTime::ZERO
        );

        let cancel_time = LogicalTime::from_nanos(1_100);
        assert_eq!(
            scheduler.register_alarm(
                detpid,
                dettid,
                cancel_time,
                LogicalTime::ZERO,
                LogicalTime::ZERO,
                Signal::SIGALRM
            ),
            (LogicalTime::from_nanos(150), LogicalTime::ZERO)
        );
        assert!(scheduler.blocked.timed_waiters.is_empty());
    }

    #[test]
    fn alarm_target_falls_back_to_surviving_process_thread() {
        let mut scheduler = Scheduler::new(&Config::default());
        let leader = DetTid::from_raw(100);
        let worker = DetTid::from_raw(101);
        scheduler.thread_tree.add_child(leader, leader, true);
        scheduler.thread_tree.add_child(leader, worker, false);
        scheduler.next_turns.insert(
            worker,
            ThreadNextTurn {
                dettid: worker,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
            },
        );

        assert_eq!(
            scheduler.select_signal_target(leader, Some(leader)),
            Some(worker)
        );
    }

    #[test]
    fn physical_exit_barrier_precedes_empty_queue_timer_fast_forward() {
        let config = Config {
            backend_reports_physical_process_exits: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&config)));
        let initial_time = global_time.lock().unwrap().as_nanos();
        let exit_deadline = initial_time + LogicalTime::from_nanos(1_000);
        let first_process = DetPid::from_raw(100);
        let second_process = DetPid::from_raw(200);
        let unrelated_process = DetPid::from_raw(300);

        assert!(scheduler.begin_physical_process_exit(first_process));
        assert!(!scheduler.begin_physical_process_exit(first_process));
        assert!(scheduler.begin_physical_process_exit(second_process));
        scheduler.register_alarm(
            first_process,
            first_process,
            initial_time,
            LogicalTime::from_nanos(1_000),
            LogicalTime::ZERO,
            Signal::SIGALRM,
        );

        assert!(scheduler.step2d_handle_empty_queue(&global_time).is_err());
        assert_eq!(
            scheduler
                .blocked
                .timed_waiters
                .iter()
                .map(|(time, _)| time)
                .collect::<Vec<_>>(),
            vec![exit_deadline]
        );
        assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);

        assert!(!scheduler.complete_physical_process_exit(unrelated_process));
        assert!(scheduler.complete_physical_process_exit(first_process));
        assert!(!scheduler.complete_physical_process_exit(first_process));
        assert!(scheduler.step2d_handle_empty_queue(&global_time).is_err());
        assert!(!scheduler.blocked.timed_waiters.is_empty());
        assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);

        assert!(scheduler.complete_physical_process_exit(second_process));
        assert!(scheduler.step2d_handle_empty_queue(&global_time).is_err());
        assert!(scheduler.blocked.timed_waiters.is_empty());
        assert_eq!(global_time.lock().unwrap().as_nanos(), exit_deadline);
    }

    #[test]
    fn physical_exit_barrier_is_disabled_for_other_backends() {
        let config = Config {
            cancel_killed_thread_rpcs: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let process = DetPid::from_raw(100);

        assert!(!scheduler.begin_physical_process_exit(process));

        assert!(scheduler.pending_physical_process_exits.is_empty());
        assert!(!scheduler.complete_physical_process_exit(process));
        assert_eq!(scheduler.release_all_physical_process_exits(), 0);
    }

    #[test]
    fn physical_exit_barrier_begins_when_last_process_thread_is_logically_dead() {
        let config = Config {
            backend_reports_physical_process_exits: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let leader = DetTid::from_raw(100);
        let worker = DetTid::from_raw(101);
        scheduler.thread_tree.add_child(leader, leader, true);
        scheduler.thread_tree.add_child(leader, worker, false);
        for dettid in [leader, worker] {
            scheduler.next_turns.insert(
                dettid,
                ThreadNextTurn {
                    dettid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                },
            );
        }

        scheduler.logically_kill_thread(&leader, &leader, MmId::initial(leader));
        assert!(scheduler.pending_physical_process_exits.is_empty());

        scheduler.logically_kill_thread(&worker, &leader, MmId::initial(leader));
        assert_eq!(
            scheduler.pending_physical_process_exits,
            BTreeSet::from([leader])
        );
    }

    #[test]
    fn final_root_and_orphan_exits_release_exact_pid_barriers() {
        let config = Config {
            backend_reports_physical_process_exits: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let root = DetPid::from_raw(100);
        let child = DetPid::from_raw(200);
        scheduler.thread_tree.add_child(root, root, true);
        scheduler.thread_tree.add_child(root, child, true);
        for dettid in [root, child] {
            scheduler.next_turns.insert(
                dettid,
                ThreadNextTurn {
                    dettid,
                    child_tid_addr: 0,
                    req: Ivar::new(),
                    resp: Ivar::new(),
                },
            );
        }

        scheduler.logically_kill_thread(&root, &root, MmId::initial(root));
        assert_eq!(
            scheduler.pending_physical_process_exits,
            BTreeSet::from([root])
        );
        assert!(!scheduler.complete_physical_process_exit(child));
        assert!(scheduler.complete_physical_process_exit(root));
        assert!(scheduler.pending_physical_process_exits.is_empty());

        scheduler.logically_kill_thread(&child, &child, MmId::initial(child));
        assert_eq!(
            scheduler.pending_physical_process_exits,
            BTreeSet::from([child])
        );
        assert!(scheduler.complete_physical_process_exit(child));
        assert!(scheduler.pending_physical_process_exits.is_empty());
    }

    #[test]
    fn final_child_exit_does_not_block_parent_timer() {
        let config = Config {
            backend_reports_physical_process_exits: true,
            ..Config::default()
        };
        let mut scheduler = Scheduler::new(&config);
        let global_time = Arc::new(Mutex::new(GlobalTime::new(&config)));
        let initial_time = global_time.lock().unwrap().as_nanos();
        let deadline = initial_time + LogicalTime::from_nanos(1_000);
        let parent = DetPid::from_raw(100);
        let child = DetPid::from_raw(200);
        scheduler.thread_tree.add_child(parent, parent, true);
        scheduler.thread_tree.add_child(parent, child, true);
        scheduler.next_turns.insert(
            parent,
            ThreadNextTurn {
                dettid: parent,
                child_tid_addr: 0,
                req: Ivar::new(),
                resp: Ivar::new(),
            },
        );
        scheduler.priorities.insert(parent, DEFAULT_PRIORITY);
        scheduler.blocked.timed_waiters.insert(deadline, parent);

        assert!(scheduler.begin_physical_process_exit(child));
        assert!(scheduler.step2d_handle_empty_queue(&global_time).is_err());
        assert_eq!(global_time.lock().unwrap().as_nanos(), initial_time);
        assert!(scheduler.complete_physical_process_exit(child));
        assert!(scheduler.pending_physical_process_exits.is_empty());
        assert!(scheduler.step2d_handle_empty_queue(&global_time).is_err());
        assert_eq!(global_time.lock().unwrap().as_nanos(), deadline);
    }

    /// Build an `HbRuntime` from a JSON happens-before spec for testing.
    fn hb_runtime(json: &str) -> HbRuntime {
        let program = detcore_model::happens_before::HappensBeforeSpec::from_json(json)
            .unwrap()
            .normalize()
            .unwrap();
        HbRuntime::new(program)
    }

    /// The enforcement predicates that drive `hb_checkpoint`: an anchor is
    /// reached at exactly its `SyscallCount` on the right thread, an AFTER anchor
    /// of a Hard edge is blocked until its BEFORE anchor fires, and firing the
    /// BEFORE anchor opens the gate. This mirrors the two-thread race validated
    /// end-to-end (main_write < worker_write forcing A before B).
    #[test]
    fn hb_runtime_gate_opens_only_after_before_anchor_fires() {
        let mut hb = hb_runtime(
            r#"{
              "version": 1,
              "threads": { "main": {"dettid": 3}, "worker": {"dettid": 5} },
              "events": {
                "main_write":   {"thread": "main",   "syscalls": 47},
                "worker_write": {"thread": "worker", "syscalls": 8}
              },
              "edges": [ {"before": "main_write", "after": "worker_write", "strength": "hard"} ]
            }"#,
        );
        let main = DetTid::from_raw(3);
        let worker = DetTid::from_raw(5);

        // Anchors resolve to the addressed (thread, count) and nothing else.
        assert_eq!(
            hb.anchors_at_syscall(main, 47),
            vec!["main_write".to_string()]
        );
        assert_eq!(
            hb.anchors_at_syscall(worker, 8),
            vec!["worker_write".to_string()]
        );
        assert!(hb.anchors_at_syscall(main, 8).is_empty());
        assert!(hb.anchors_at_syscall(worker, 47).is_empty());
        assert!(hb.anchors_at_syscall(worker, 7).is_empty());

        // Before its gating BEFORE anchor fires, the AFTER anchor is blocked and
        // the BEFORE anchor is free (it gates nothing).
        assert!(hb.anchor_blocked("worker_write"));
        assert!(!hb.anchor_blocked("main_write"));

        // Firing the BEFORE anchor opens the gate exactly once.
        assert!(hb.fired.insert("main_write".to_string()));
        assert!(!hb.anchor_blocked("worker_write"));
    }

    /// A soft edge never parks its AFTER thread, and `spawn_ordinal` addressing
    /// resolves against the observed spawn order (index 0 = root).
    #[test]
    fn hb_runtime_soft_edge_and_spawn_ordinal_resolution() {
        let mut hb = hb_runtime(
            r#"{
              "version": 1,
              "threads": {
                "root":  {"spawn_ordinal": 0},
                "child": {"spawn_ordinal": 1}
              },
              "events": {
                "a": {"thread": "root",  "syscalls": 3},
                "b": {"thread": "child", "syscalls": 4}
              },
              "edges": [ {"before": "a", "after": "b", "strength": "soft"} ]
            }"#,
        );
        // A soft edge biases but never hard-blocks, so its AFTER is never parked.
        assert!(!hb.anchor_blocked("b"));

        // spawn_ordinal is unresolved until threads are observed at creation time.
        let root = DetTid::from_raw(3);
        let child = DetTid::from_raw(5);
        assert!(hb.anchors_at_syscall(root, 3).is_empty());
        hb.note_spawn(root); // index 0 -> root
        hb.note_spawn(child); // index 1 -> first spawned child
        assert_eq!(hb.anchors_at_syscall(root, 3), vec!["a".to_string()]);
        assert_eq!(hb.anchors_at_syscall(child, 4), vec!["b".to_string()]);
        // note_spawn is idempotent, so a re-registration does not shift indices.
        hb.note_spawn(root);
        assert_eq!(hb.anchors_at_syscall(child, 4), vec!["b".to_string()]);
    }
}
