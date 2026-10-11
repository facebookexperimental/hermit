/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The main queue for runnable tasks.
//!
//! Tasks are selected from the queue based on 2 factors:
//! 1. Their priority
//! 2. Their round-robin order.
//!
//! The factors are compared in that order. Round-robin orders monotonically increase across
//! the entire queue; a task is assigned an order at insertion time.
//!
//! Round-robin orders can also be negative when the `push_front` method is
//! used. This "skips the line" within the priority level. This is mostly
//! relevant in non-chaos modes, where all threads have the same priority. In
//! this case, time-based events should skip the line and end-up in the front of
//! the queue.
//!
//! In addition, some priority values are reserved, such as a high priority
//! for IO eager polling.
//!
//! # Polling strategy
//!
//! We employ a polling strategy for guest threads that *would* blocked, but where we
//! don't model precisely what conditions they're waiting for.  Anywhere we have a precise
//! model of inter-thread dependencies (e.g. futexes), we can sleep a thread until we
//! encounter the matching event that will wake it.  But for Linux features that we don't
//! model 100% precisely, polling is a way to remain agnostic as to the exact
//! dependencies, but still support these blocking behaviors deterministically.
//!
//! The older DetTrace system used polling, but it would poll every time through the
//! round robin queue, which can create extremely bad performance with many threads
//! polling an unbounded number of times.  We can greatly improve the performance by
//! polling only at less-frequent, but still deterministically-defined intervals, such
//! as when we think we're out of "productive" work to do.
//!
//! Thererefore we have special handling for scheduling polling tasks. When
//! initially queued, there is exponential backoff in priority with the number
//! of attempts. After enough queueing operations are performed, however,
//! polling tasks are upgraded to their original priority to prevent complete
//! starvation. The frequency of upgrades is controlled by `POLLING_UPGRADE_INTERVAL`.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Display;

use rand::RngExt as _;
use rand::SeedableRng;
use rand::distr::uniform::SampleUniform;
use rand_pcg::Pcg64Mcg;

use crate::config::SchedHeuristic;
use crate::detlog;
use crate::types::DetTid;

/// The user-accessible priority of a thread. Lowest runs first.
pub type Priority = u64;

const EAGER_IO_REPOLL_PRIORITY: Priority = Priority::MIN;

/// The lowest/highest priority a thread can have.
pub const FIRST_PRIORITY: Priority = EAGER_IO_REPOLL_PRIORITY + 1;

/// The last/lowest (numerically largest) priority a thread can have.
pub const LAST_PRIORITY: Priority = 10000;

/// A high priority for threads the replayer DOES want to run.
pub const REPLAY_FOREGROUND_PRIORITY: Priority = FIRST_PRIORITY;

/// A low priority given to threads the replayer does NOT want to run.
pub const REPLAY_DEFERRED_PRIORITY: Priority = LAST_PRIORITY - 1;

/// The default priority for a thread. If chaos mode is not enabled, all threads have this
/// priority.
pub const DEFAULT_PRIORITY: Priority = 1000;

/// Whether the priority is user-accessible. We use some values for special
/// purposes; these shouldn't be set by the user.
pub fn is_ordinary_priority(prio: Priority) -> bool {
    (FIRST_PRIORITY..=LAST_PRIORITY).contains(&prio)
}

/// Deterministically transform 64 bits of entropy into a random user-settable
/// priority.
pub fn entropy_to_priority(entropy: u64) -> Priority {
    let range = LAST_PRIORITY - FIRST_PRIORITY + 1;
    let offset = entropy % range;
    FIRST_PRIORITY + offset
}

/// How many chaos `sched_yield` turns that pass another runnable thread over
/// a thread may take, since the yield epoch last ended, before its next one
/// joins the epoch ([`RunQueue::push_yielded_into_epoch`]). One plain yield
/// keeps the schedules in which a yielder runs again before a thread that
/// just became runnable (a futex waiter woken for a signal, for example);
/// joining the epoch at the second bounds how long a spinner can hold a later
/// band off (<https://github.com/rrnewton/hermit/issues/4068>).
pub const PLAIN_YIELDS_BEFORE_EPOCH: u32 = 1;

/// The round robin turn of threads within a given priority level. Lowest runs
/// first. Both negative and positive values are used to allow insertion of a
/// thread at both the "front" and "back" of a priority level.
type RoundRobinTurn = i64;

/// The key into the priority queue that uniquely determines what to run next.
/// Priorities that compare lower run first.
#[derive(Debug, Copy, Clone)]
pub struct PrioritizedOrder {
    priority: Priority,
    turn: RoundRobinTurn,
}

// These match the derived definitions, but clearly display our intention:
impl Ord for PrioritizedOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then(self.turn.cmp(&other.turn))
    }
}
impl PartialOrd for PrioritizedOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for PrioritizedOrder {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for PrioritizedOrder {}

impl fmt::Display for PrioritizedOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(p: {}, t: {})", self.priority, self.turn)
    }
}

/// The outcome of [`RunQueue::restore_poller_priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollerRestore {
    /// The thread was queued as a backed-off poller and now has its normal priority.
    Restored,
    /// The thread is queued, but not backed off: a poll upgrade or a priority
    /// change has already restored it.
    NotBackedOff,
    /// The thread is not in the run queue.
    NotQueued,
}

/// After queueing this many new tasks, perform a "poll upgrade," in which
/// we upgrade all outstanding polling tasks their original priority levels,
/// temporarily negating backoff behavior.
const POLLING_UPGRADE_INTERVAL: u64 = 200;

#[derive(Debug, Copy, Clone)]
struct QueueValue {
    tid: DetTid,
    /// Upgrade to this priority during polling upgrades
    poll_upgrade: Option<Priority>,
    /// Set when `restore_poller_priority` restored this poller because its
    /// timeout deadline passed, and the poller was at `LAST_PRIORITY` while
    /// backed off. Until it runs, the deterministic-work-first gates
    /// (`only_pollers`, `has_runnable_besides`) keep counting it as a poller,
    /// exactly as they did while it was backed off. A poller backed off below
    /// `LAST_PRIORITY` (after 1 to 3 retries) was already ordinary work for
    /// the gates, and stays so
    /// (<https://github.com/rrnewton/hermit/issues/3952>).
    deadline_restored: bool,
}

impl QueueValue {
    /// Whether this entry counts as a poller for the deterministic-work-first
    /// gates, given its queue priority.
    fn counts_as_poller(&self, priority: Priority) -> bool {
        priority >= LAST_PRIORITY || self.deadline_restored
    }

    /// Whether this entry is a polling thread whose backoff has reached the
    /// last band ([`RunQueue::push_poller`]): only then does it stop holding a
    /// yield epoch open.
    fn is_fully_backed_off_poller(&self, priority: Priority) -> bool {
        self.poll_upgrade.is_some() && priority >= LAST_PRIORITY
    }
}

/// One suspended queue entry. Global yield/random-selection state stays live.
#[derive(Debug, Clone)]
pub(super) struct SuspendedRunQueueEntry {
    key: PrioritizedOrder,
    value: QueueValue,
    persistent_priority: Priority,
}

#[derive(Debug, Clone)]
pub struct RunQueue {
    /// We use a "flattened" queue (rather than a Priority -> Vec<DetTid> map)
    /// to simplify peek/pop logic: there's no need to ignore clear empty
    /// from unused priority levels vectors. This could also reduce allocator
    /// pressure. Also, each thread having a clear global key makes it easier to
    /// change their priorities after they are in the queue.
    ///
    /// Additionally, we use a TreeMap rather than a Heap to ease removing /
    /// inserting random values for poll upgrades. std::BinaryHeap would require
    /// destroying/re-allocating the entire structure to do this.
    queue: BTreeMap<PrioritizedOrder, QueueValue>,

    // We use global turn counters across all priority levels. This foregoes
    // the need for an extra data structure to track them, while also ensuring
    // unique keys for every insertion. Because of this, we never need to alter
    // a turn value when altering the priority level of a thread.
    last_back_turn: RoundRobinTurn,
    last_front_turn: RoundRobinTurn,

    /// Used to lock the queue from other changes while we are tentatively popping from it, and also
    /// cache the result.
    tentative_selection: Option<DetTid>,
    tentative_selection_is_exact: bool,

    /// A thread that explicitly yielded must not be selected again until some
    /// other runnable thread receives a turn.
    yielded_skip: Option<DetTid>,

    /// The threads whose chaos `sched_yield` streak passed
    /// [`PLAIN_YIELDS_BEFORE_EPOCH`] since the last turn of a thread that did
    /// not yield ([`Self::push_yielded_into_epoch`]). While any queued thread
    /// outside the set is not a fully backed-off poller, selection skips its
    /// members. A turn
    /// in which a thread that did not yield ran empties it
    /// ([`Self::end_yield_epoch`]): a completed guest turn (a SaBRe loopback
    /// poll's yield included), a background grant and a priority change point.
    /// A poller's non-committing backoff does not. Covering every queued
    /// thread empties it too, as does leaving only fully backed-off pollers
    /// outside it, and a
    /// thread leaves it when it leaves the queue. Bands and queue keys are
    /// untouched.
    yield_epoch: Option<BTreeSet<DetTid>>,
    /// Per thread, the chaos `sched_yield` turns since the yield epoch last
    /// ended that were taken while another thread was queued, so that the
    /// yield passed it over. The first
    /// [`PLAIN_YIELDS_BEFORE_EPOCH`] are plain requeues; the next joins the
    /// epoch.
    yield_streaks: BTreeMap<DetTid, u32>,
    /// Every queued thread that is queued because it yielded (`push_yielded`)
    /// and has not run since. Unlike `yielded_skip`, which only the latest
    /// yielder holds, this keeps every one of several threads spinning on
    /// `sched_yield` (`only_pollers_or_yielders`).
    queued_by_yield: BTreeSet<DetTid>,

    // TODO: The following fields need to be properly abstracted into separate types of run queues.
    /// Which scheduling strategy shall we use.
    sched_strategy: SchedHeuristic,
    prng: Pcg64Mcg,

    sticky_random_param: f64,
    sticky_random_selection: Option<DetTid>,
}

/// A multi-line print of the runqueue.
impl fmt::Display for RunQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Run queue, size={}, last_back_turn={}, last_front_turn={}:",
            self.queue.len(),
            self.last_back_turn,
            self.last_front_turn,
        )?;
        for x in self.queue.iter() {
            writeln!(f, "    {:.500?}", x)?;
        }
        Ok(())
    }
}

impl RunQueue {
    /// Create a new RunQueue.
    pub fn new(ss: SchedHeuristic, seed: u64, srp: f64) -> Self {
        detlog!("SCHEDRAND: seeding scheduler runqueue with seed {}", seed);
        Self {
            queue: BTreeMap::new(),
            // For clarity, 0 is unused so that positive/negative == back/front:
            last_back_turn: 0,
            last_front_turn: 0,
            sched_strategy: ss,
            tentative_selection: None,
            tentative_selection_is_exact: false,
            yielded_skip: None,
            yield_epoch: None,
            yield_streaks: BTreeMap::new(),
            queued_by_yield: BTreeSet::new(),
            prng: Pcg64Mcg::seed_from_u64(seed),
            sticky_random_param: srp,
            sticky_random_selection: None,
        }
    }

    fn push_safety_check(&self, tid: DetTid) {
        if cfg!(debug_assertions) {
            // Expensive.
            for qv in self.queue.values() {
                if qv.tid == tid {
                    panic!(
                        "Invariant violation! Tried to add {} to runqueue, but it's already present:\n {:?}",
                        tid, self
                    );
                }
            }
        }
    }

    /// True if any thread other than `exclude` is runnable at ordinary
    /// (non-poller) priority, where a thread `restore_poller_priority` restored
    /// still counts as a poller until it runs. This is the "deterministic work still runnable"
    /// test used to decide whether an asynchronous signal delivery must defer to
    /// guest work that was already scheduled. Read-only, so it is safe to call
    /// while a tentative_pop selection is in progress.
    pub fn has_runnable_besides(&self, exclude: DetTid) -> bool {
        self.queue
            .iter()
            .any(|(k, v)| v.tid != exclude && !v.counts_as_poller(k.priority))
    }

    /// True when every queued thread is a poller (or the queue is empty): the
    /// "no deterministic work is runnable" test of the deterministic-work-first
    /// gates. A poller is an entry at `LAST_PRIORITY`, or one that
    /// `restore_poller_priority` restored and that has not run since. Read-only.
    pub fn only_pollers(&self) -> bool {
        self.queue
            .iter()
            .all(|(k, v)| v.counts_as_poller(k.priority))
    }

    /// `only_pollers`, also counting every thread queued because it yielded
    /// and not run since (`queued_by_yield`). A thread that calls
    /// `sched_yield` asks for another thread to run first, so, like a poller,
    /// it is not work that a finished background call must wait behind.
    /// Counting it as such work starved the background call forever whenever
    /// a sibling spun on `sched_yield`
    /// (<https://github.com/rrnewton/hermit/issues/4092>), and counting all but
    /// the latest yielder did the same for two spinners, which keep clearing
    /// each other's `yielded_skip` (Claude review of
    /// <https://github.com/rrnewton/hermit/pull/4120>). Read-only.
    pub fn only_pollers_or_yielders(&self) -> bool {
        self.queue
            .iter()
            .all(|(k, v)| v.counts_as_poller(k.priority) || self.queued_by_yield.contains(&v.tid))
    }

    /// True while a `tentative_pop`/commit transaction is underway, i.e. the
    /// daemon has peeked a selection and may have released the scheduler lock
    /// across an await. Fatal backend publication uses this read-only query to
    /// close the transaction once before waking consuming cleanup. Ordinary
    /// run-queue admissions and removals are always buffered and
    /// applied at the deterministic `step2` drain (window guaranteed closed), so
    /// a handler never needs to ask whether a window is live — see
    /// `Scheduler::admit_to_run_queue` / `deschedule_or_defer`. Read-only.
    pub fn tentative_pop_in_progress(&self) -> bool {
        self.tentative_selection.is_some()
    }

    /// Push a thread to the back of the specified priority. Return the
    /// resulting overall position in the queue.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_back(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        if !is_ordinary_priority(priority) {
            panic!("This is not an acceptable priority value: {}", priority);
        }
        self.push_back_inner(tid, priority, None)
    }

    /// Requeue an explicitly yielding thread at its persistent priority while
    /// excluding it from the next selection. The exclusion, rather than the
    /// queue key, makes this a one-turn operation under every heuristic.
    pub fn push_yielded(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        assert!(self.yielded_skip.is_none());
        self.yielded_skip = Some(tid);
        self.queued_by_yield.insert(tid);
        self.push_back(tid, priority)
    }

    /// Requeue a thread that called `sched_yield` under chaos at its own
    /// priority. If another thread is queued, the yield passes it over and
    /// counts toward the thread's streak; once the streak
    /// exceeds [`PLAIN_YIELDS_BEFORE_EPOCH`], the thread joins the epoch,
    /// beginning one if none is open. A yield made with no one else to run
    /// does not count, so a thread that spins alone has no streak when another
    /// becomes runnable. Its turn must have been committed with
    /// [`Self::commit_tentative_pop_yield`]. Unlike [`Self::push_yielded`], the
    /// epoch lasts until a thread that did not yield has a turn, however many
    /// yielders take turns in between, and it changes neither the thread's
    /// band nor its queue key.
    pub fn push_yielded_into_epoch(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        // Any other queued thread counts, an epoch member included: a thread
        // left outside an epoch of yielders must be able to join it, or it
        // would spin ahead of them forever.
        let skipped_someone = self.queue.values().any(|value| value.tid != tid);
        if skipped_someone {
            let streak = self.yield_streaks.entry(tid).or_insert(0);
            *streak += 1;
            if *streak > PLAIN_YIELDS_BEFORE_EPOCH {
                self.yield_epoch
                    .get_or_insert_with(BTreeSet::new)
                    .insert(tid);
            }
        }
        self.push_back(tid, priority)
    }

    /// Push a polling thread. The priority level is an exponential backoff from
    /// the given `normal_priority` value. The pushed thread will also
    /// participatein "poll upgrades" in which periodically polling threads are
    /// re-boosted to their original `normal_priority` values.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_poller(
        &mut self,
        tid: DetTid,
        normal_priority: Priority,
        poll_attempt: u32,
    ) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        // Exponential backoff in priority, up to LAST_PRIORITY:
        let priority = 1u64
            .checked_shl(poll_attempt)
            .and_then(|f| f.checked_mul(normal_priority))
            .unwrap_or(Priority::MAX)
            .min(LAST_PRIORITY);
        // Upgrade back to original priority:
        self.push_back_inner(tid, priority, Some(normal_priority))
    }

    fn push_back_inner(
        &mut self,
        tid: DetTid,
        priority: Priority,
        poll_upgrade: Option<Priority>,
    ) -> PrioritizedOrder {
        self.last_back_turn += 1;
        let turn = self.last_back_turn;
        let prio = PrioritizedOrder { priority, turn };
        self.push_inner(tid, prio, poll_upgrade)
    }

    /// Push a thread to the front of the specified priority. `push_back` should
    /// be used unless special circumstances call for `push_front`. Return the
    /// resulting overall position in the queue.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_front(&mut self, tid: DetTid, priority: Priority) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        assert!(is_ordinary_priority(priority));
        self.push_front_inner(tid, priority, None)
    }

    /// Workaround for eager io repolling: this will send the thread to the
    /// absolute front of the queue. Return the resulting overall position in
    /// the queue.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn push_eager_io_repoll(&mut self, tid: DetTid) -> PrioritizedOrder {
        assert!(self.tentative_selection.is_none());
        self.push_safety_check(tid);
        let priority = EAGER_IO_REPOLL_PRIORITY;
        self.push_front_inner(tid, priority, None)
    }

    fn push_front_inner(
        &mut self,
        tid: DetTid,
        priority: Priority,
        poll_upgrade: Option<Priority>,
    ) -> PrioritizedOrder {
        self.last_front_turn -= 1;
        let turn = self.last_front_turn;
        let prio = PrioritizedOrder { priority, turn };
        self.push_inner(tid, prio, poll_upgrade)
    }

    fn push_inner(
        &mut self,
        tid: DetTid,
        prio: PrioritizedOrder,
        poll_upgrade: Option<Priority>,
    ) -> PrioritizedOrder {
        let qval = QueueValue {
            tid,
            poll_upgrade,
            deadline_restored: false,
        };
        let old = self.queue.insert(prio, qval);
        assert!(old.is_none()); // last_*_turn should be monotonic
        self.check_poll_upgrade();
        prio
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn tids(&self) -> impl Iterator<Item = &DetTid> {
        self.queue.values().map(|v| &v.tid)
    }

    /// Read-only: this is ok while locked by tentative_pop.
    pub fn contains_tid(&self, tid: DetTid) -> bool {
        self.tids().any(|t| t == &tid)
    }

    pub(super) fn suspend(
        &mut self,
        tid: DetTid,
        persistent_priority: Priority,
    ) -> Option<SuspendedRunQueueEntry> {
        assert!(self.tentative_selection.is_none());
        let key = *self.queue.iter().find(|(_, v)| v.tid == tid)?.0;
        let value = self.queue.remove(&key).expect("located queue entry");
        Some(SuspendedRunQueueEntry {
            key,
            value,
            persistent_priority,
        })
    }

    pub(super) fn restore(
        &mut self,
        mut entry: SuspendedRunQueueEntry,
        current_priority: Priority,
    ) {
        assert!(self.tentative_selection.is_none());
        assert!(!self.contains_tid(entry.value.tid));
        if entry.persistent_priority != current_priority {
            entry.key.priority = current_priority;
            if entry.value.poll_upgrade.is_some() {
                entry.value.poll_upgrade = Some(current_priority);
            }
        }
        assert!(self.queue.insert(entry.key, entry.value).is_none());
    }

    /// Remove `tid` from the queue, returning true if removal ocurred.
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn remove_tid(&mut self, tid: DetTid) -> bool {
        assert!(self.tentative_selection.is_none());

        // This is O(N), but could be faster if we also stored a thread -> priority mapping.
        let mut kept_all = true;
        self.queue.retain(|_k, v| {
            let ret = v.tid != tid;
            kept_all = kept_all && ret;
            ret
        });
        if self.yielded_skip == Some(tid) {
            self.yielded_skip = None;
        }
        if let Some(epoch) = &mut self.yield_epoch {
            epoch.remove(&tid);
        }
        self.yield_streaks.remove(&tid);
        self.queued_by_yield.remove(&tid);
        // A successful nonleader exec can remove one thread incarnation and
        // admit its replacement under the same raw TID in a single scheduler
        // drain. Do not let the replacement inherit the destroyed leader's
        // sticky-random selection.
        if self.sticky_random_selection == Some(tid) {
            self.sticky_random_selection = None;
        }
        !kept_all
    }

    // Helper function for logging purposes.
    fn random_range<T>(&mut self, start: T, end: T) -> T
    where
        T: SampleUniform + Display + PartialOrd + Copy,
    {
        let r = self.prng.random_range(start..end);
        detlog!("SCHEDRAND: [{},{}) => {}", start, end, r);
        r
    }

    /// Begin, but do not complete, a pop_transaction.  This can be committed or undone later.  But
    /// one of those must happen before other modification operations can occur on the RunQueue.
    ///
    /// Postcondition: if return a `Some` value, the RunQueue enters a *locked* state where
    /// commit or undo must happen before any other mutations to the structure.
    pub fn tentative_pop_next(&mut self) -> Option<DetTid> {
        assert!(!self.tentative_selection_is_exact);
        let skip = self
            .yielded_skip
            .filter(|tid| self.queue.len() > 1 && self.contains_tid(*tid));
        // A yield epoch is over when it covers every queued thread, or when
        // every queued thread outside it is a poller backed off all the way:
        // a poller's backoff turns do not commit, so do not end the epoch, and
        // one waiting on a yielder must not hold the yielders off forever. A
        // thread merely in the last band (as `--chaos-target-races` places
        // half of all new threads) is not a poller.
        if self.yield_epoch.as_ref().is_some_and(|epoch| {
            self.queue.iter().all(|(key, value)| {
                epoch.contains(&value.tid) || value.is_fully_backed_off_poller(key.priority)
            })
        }) {
            self.end_yield_epoch();
        }
        let mut excluded: BTreeSet<DetTid> = skip.into_iter().collect();
        if let Some(epoch) = &self.yield_epoch {
            let with_epoch: BTreeSet<DetTid> = excluded.union(epoch).copied().collect();
            // The epoch never covers every queued thread here, but a one-turn
            // skip outside it could make up the rest: then only the skip holds.
            if self
                .queue
                .values()
                .any(|value| !with_epoch.contains(&value.tid))
            {
                excluded = with_epoch;
            }
        }
        let excluded_count = self
            .queue
            .values()
            .filter(|value| excluded.contains(&value.tid))
            .count();
        self.tentative_selection = match self.sched_strategy {
            SchedHeuristic::None | SchedHeuristic::ConnectBind => self
                .queue
                .values()
                .find(|value| !excluded.contains(&value.tid))
                .map(|value| value.tid),
            SchedHeuristic::Random => {
                if self.queue.is_empty() {
                    return None;
                }

                // If there is not Tid picked from a previous operation, let's pick one now.
                if self.tentative_selection.is_none() {
                    let eligible = self.queue.len() - excluded_count;
                    let random_idx = self.random_range(0, eligible);
                    self.tentative_selection = self
                        .queue
                        .values()
                        .filter(|value| !excluded.contains(&value.tid))
                        .nth(random_idx)
                        .map(|value| value.tid);
                };

                self.tentative_selection
            }
            SchedHeuristic::StickyRandom => {
                if self.queue.is_empty() {
                    return None;
                }

                if self
                    .sticky_random_selection
                    .is_some_and(|tid| excluded.contains(&tid))
                {
                    self.sticky_random_selection = None;
                }
                if self.sticky_random_selection.is_none()
                    || !self.contains_tid(self.sticky_random_selection.unwrap())
                {
                    let eligible = self.queue.len() - excluded_count;
                    let random_idx = self.random_range(0, eligible);
                    self.sticky_random_selection = self
                        .queue
                        .values()
                        .filter(|value| !excluded.contains(&value.tid))
                        .nth(random_idx)
                        .map(|value| value.tid);
                }

                self.sticky_random_selection
            }
        };

        self.tentative_selection
    }

    // TODO-HUMAN-REVIEW(PR-868): Review exact run-queue selection for vfork barriers.
    /// Begin a pop transaction for one specific queued thread, bypassing the
    /// configured scheduling heuristic without changing the thread's priority.
    pub fn tentative_pop_tid(&mut self, tid: DetTid) -> Option<DetTid> {
        assert!(self.tentative_selection.is_none());
        if self.contains_tid(tid) {
            self.tentative_selection = Some(tid);
            self.tentative_selection_is_exact = true;
        }
        self.tentative_selection
    }

    /// Complete the tentative pop operation, readying the RunQueue for future operations.  This
    /// operation is only permissible when the queue is locked, i.e. the tentative_pop has
    /// previously returned `Some`.
    /// Commit the queue transaction alone. It neither consumes the one-turn
    /// exclusion nor ends the yield epoch: a poller's non-committing backoff
    /// uses it, and a priority change point ends the epoch itself.
    pub fn commit_tentative_pop(&mut self) -> DetTid {
        self.commit_selection()
    }

    /// End the yield epoch: a thread that did not yield has had a turn.
    pub fn end_yield_epoch(&mut self) {
        self.yield_epoch = None;
        self.yield_streaks.clear();
    }

    /// Commit the turn of a thread that called `sched_yield` under chaos,
    /// keeping the yield epoch it is about to join
    /// ([`Self::push_yielded_into_epoch`]).
    pub fn commit_tentative_pop_yield(&mut self) -> DetTid {
        let tid = self.commit_selection();
        self.consume_yield_exclusion();
        tid
    }

    fn commit_selection(&mut self) -> DetTid {
        // Check that queue is locked and unlock it.
        let tentative_selection = self
            .tentative_selection
            .take()
            .expect("tentative_pop to already returned a `Some`");
        let exact = std::mem::take(&mut self.tentative_selection_is_exact);

        let ret = if exact {
            let key = *self
                .queue
                .iter()
                .find(|(_key, value)| value.tid == tentative_selection)
                .map(|(key, _value)| key)
                .unwrap();
            self.queue.remove(&key).map(|value| value.tid)
        } else {
            match self.sched_strategy {
                SchedHeuristic::None | SchedHeuristic::ConnectBind | SchedHeuristic::Random => {
                    let key = *self
                        .queue
                        .iter()
                        .find(|(_k, v)| v.tid == tentative_selection)
                        .map(|(k, _v)| k)
                        .unwrap();
                    self.queue.remove(&key).map(|v| v.tid)
                }
                SchedHeuristic::StickyRandom => {
                    let tid = self.sticky_random_selection.unwrap();
                    // Probability of staying to our current thread on the next round.
                    // If the generated random number is smaller than what we set, we switch threads.
                    if self.random_range(0f64, 1f64) <= 1.0 - self.sticky_random_param {
                        self.sticky_random_selection = None;
                    }

                    let key = *self
                        .queue
                        .iter()
                        .find(|(_k, v)| v.tid == tid)
                        .map(|(k, _v)| k)
                        .unwrap();

                    self.queue.remove(&key).map(|v| v.tid)
                }
            }
        }
        .expect("to always return a DetTid");
        // The above should always return a DetTid as we peeked right before.
        // If this invariant is violated, then it's a bug, or the queue is modified
        // between the peek and tentative_pop and commit_tentative_pop.
        debug_assert!(ret == tentative_selection);
        self.queued_by_yield.remove(&ret);
        ret
    }

    /// Commit a tentative pop for a guest turn that will actually run. A
    /// yielded thread's one-turn exclusion is consumed only here, not by
    /// scheduler bookkeeping turns that never unblock a guest.
    pub fn commit_tentative_pop_completed_turn(&mut self) -> DetTid {
        let tid = self.commit_tentative_pop();
        self.consume_yield_exclusion();
        // A completed guest turn that was not a chaos `sched_yield` (those use
        // `commit_tentative_pop_yield`) ends the yield epoch. That includes a
        // SaBRe loopback poll's yield: its count is host-timed, but a loopback
        // poller may be waiting on a yielder, so it must not be held behind
        // the epoch.
        self.end_yield_epoch();
        tid
    }

    /// Mark that a different guest received execution after an explicit yield.
    pub fn consume_yield_exclusion(&mut self) {
        self.yielded_skip = None;
    }

    /// Forget the tentative pop as though it never happened.
    pub fn undo_tentative_pop(&mut self) {
        assert!(self.tentative_selection.is_some());
        self.tentative_selection = None;
        self.tentative_selection_is_exact = false;
    }

    /// Return how many things have been queued.
    fn turn_counter(&self) -> u64 {
        debug_assert!(self.last_back_turn >= 0);
        debug_assert!(self.last_front_turn <= 0);
        self.last_back_turn as u64 + self.last_front_turn.unsigned_abs()
    }

    fn check_poll_upgrade(&mut self) {
        if self.turn_counter().is_multiple_of(POLLING_UPGRADE_INTERVAL) {
            self.do_poll_upgrade()
        }
    }

    /// Whether selection follows priority order: `None` and `ConnectBind` pop
    /// the first queued entry, so a backed-off poller can be starved; `Random`
    /// and `StickyRandom` draw a uniform index into the queue and never are.
    pub fn is_priority_ordered(&self) -> bool {
        matches!(
            self.sched_strategy,
            SchedHeuristic::None | SchedHeuristic::ConnectBind
        )
    }

    /// The priority `tid` is queued at, if it is queued.
    #[cfg(test)]
    pub fn queued_priority(&self, tid: DetTid) -> Option<Priority> {
        self.queue
            .iter()
            .find(|(_, value)| value.tid == tid)
            .map(|(key, _)| key.priority)
    }

    /// Restore one backed-off poller to the normal priority recorded when it
    /// was queued, and move it to the back of that priority level, as a push
    /// does (<https://github.com/rrnewton/hermit/issues/3952>). At the back,
    /// it runs after every thread that was already runnable when its deadline
    /// passed, as it would have while backed off, and before any thread pushed
    /// later, such as a spinner requeued at its next preemption. Keeping its
    /// older round-robin turn instead let it overtake a thread that became
    /// runnable before its deadline. The move is a queueing operation: it
    /// takes a new back turn and counts toward the poll-upgrade interval.
    ///
    /// Mutating operation: this will error if a tentative_pop/commit transaction is underway.
    pub fn restore_poller_priority(&mut self, tid: DetTid) -> PollerRestore {
        assert!(self.tentative_selection.is_none());
        let Some((&key, value)) = self.queue.iter().find(|(_, value)| value.tid == tid) else {
            return PollerRestore::NotQueued;
        };
        let Some(normal_priority) = value.poll_upgrade else {
            return PollerRestore::NotBackedOff;
        };
        let mut value = self.queue.remove(&key).expect("located queue entry");
        value.poll_upgrade = None;
        // The gates count a backed-off poller as a poller only at
        // LAST_PRIORITY; keep exactly that classification until it runs.
        value.deadline_restored = key.priority >= LAST_PRIORITY;
        self.last_back_turn += 1;
        let restored = PrioritizedOrder {
            priority: normal_priority,
            turn: self.last_back_turn,
        };
        assert!(self.queue.insert(restored, value).is_none());
        self.check_poll_upgrade();
        PollerRestore::Restored
    }

    /// Upgrade polled tasks to their specified normal priority.
    #[cold]
    fn do_poll_upgrade(&mut self) {
        let upgrades = self
            .queue
            .iter()
            .filter_map(|(k, v)| v.poll_upgrade.map(|upgd| (*k, upgd)))
            .collect::<Vec<(PrioritizedOrder, Priority)>>();
        // TODO(T100400409): if all polling threads are below a certain priority, this
        // can use a range query rather than iterating over all threads in the
        // run queue:
        for (key, upgrade_prio) in upgrades {
            let mut new_key = key;
            new_key.priority = upgrade_prio;
            let mut qval = self.queue.remove(&key).unwrap();
            qval.poll_upgrade = None; // there's no need to upgrade to the same priority twice
            let old = self.queue.insert(new_key, qval);
            assert!(old.is_none()); // round robin turns should ensure uniqueness
        }
        // An upgrade makes every backed-off poller ordinary work for the
        // deterministic-work-first gates. A poller its deadline restored
        // earlier would have been upgraded here too, so it stops counting as
        // a poller now as well (https://github.com/rrnewton/hermit/issues/3952).
        for value in self.queue.values_mut() {
            value.deadline_restored = false;
        }
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new(SchedHeuristic::None, 0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `restore_poller_priority` restores one backed-off poller to its recorded
    /// priority at the back of that level: behind a thread queued before the
    /// restore, ahead of one pushed after it, such as a spinner requeued at its
    /// next preemption. Other pollers stay backed off, and the restore counts
    /// as one push (https://github.com/rrnewton/hermit/issues/3952).
    #[test]
    fn restore_poller_priority_moves_one_poller_to_the_back_of_its_level() {
        let poller = DetTid::from_raw(1);
        let other = DetTid::from_raw(2);
        let spinner = DetTid::from_raw(3);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 0.0);
        queue.push_poller(poller, DEFAULT_PRIORITY, 3);
        queue.push_poller(other, DEFAULT_PRIORITY, 3);
        queue.push_back(spinner, DEFAULT_PRIORITY);
        let pushes = queue.turn_counter();
        // Backed off, the poller loses to the spinner.
        assert_eq!(queue.tentative_pop_next(), Some(spinner));
        queue.undo_tentative_pop();

        assert_eq!(
            queue.restore_poller_priority(poller),
            PollerRestore::Restored
        );
        assert_eq!(queue.turn_counter(), pushes + 1, "a restore is one push");
        assert_eq!(queue.queued_priority(poller), Some(DEFAULT_PRIORITY));
        assert_eq!(queue.tentative_pop_next(), Some(spinner), "queued first");
        assert_eq!(queue.commit_tentative_pop(), spinner);
        queue.push_back(spinner, DEFAULT_PRIORITY);
        assert_eq!(
            queue.tentative_pop_next(),
            Some(poller),
            "ahead of the requeue"
        );
        queue.undo_tentative_pop();
        let (other_key, other_value) = queue
            .queue
            .iter()
            .find(|(_, value)| value.tid == other)
            .unwrap();
        assert_eq!(other_key.priority, 8 * DEFAULT_PRIORITY);
        assert_eq!(other_value.poll_upgrade, Some(DEFAULT_PRIORITY));

        assert_eq!(
            queue.restore_poller_priority(poller),
            PollerRestore::NotBackedOff
        );
        assert_eq!(
            queue.restore_poller_priority(spinner),
            PollerRestore::NotBackedOff
        );
        assert_eq!(
            queue.restore_poller_priority(DetTid::from_raw(9)),
            PollerRestore::NotQueued
        );
    }

    /// A poller backed off to `LAST_PRIORITY` and restored by its deadline
    /// still counts as a poller for the deterministic-work-first gates until
    /// it runs; requeued after its turn, it is ordinary work. One backed off
    /// below `LAST_PRIORITY` was ordinary work for the gates already, and
    /// stays so after the restore, as on main.
    #[test]
    fn a_deadline_restored_poller_counts_as_a_poller_until_it_runs() {
        let poller = DetTid::from_raw(1);
        let other = DetTid::from_raw(2);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 0.0);
        queue.push_poller(poller, DEFAULT_PRIORITY, 30);
        assert!(queue.only_pollers());
        assert_eq!(
            queue.restore_poller_priority(poller),
            PollerRestore::Restored
        );
        assert!(queue.only_pollers(), "restored, but it has not run");
        assert!(!queue.has_runnable_besides(other));

        assert_eq!(queue.tentative_pop_next(), Some(poller));
        assert_eq!(queue.commit_tentative_pop(), poller);
        queue.push_back(poller, DEFAULT_PRIORITY);
        assert!(!queue.only_pollers(), "after its turn it is ordinary work");
        assert!(queue.has_runnable_besides(other));

        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 0.0);
        queue.push_poller(poller, DEFAULT_PRIORITY, 1);
        assert!(
            !queue.only_pollers(),
            "at 2000 it is ordinary work, as on main"
        );
        assert_eq!(
            queue.restore_poller_priority(poller),
            PollerRestore::Restored
        );
        assert!(!queue.only_pollers(), "and stays so after the restore");
        assert!(queue.has_runnable_besides(other));
    }

    /// A poll upgrade turns every backed-off poller into ordinary work for the
    /// gates; a deadline-restored poller stops counting as a poller at the
    /// same point.
    #[test]
    fn a_poll_upgrade_ends_a_restored_pollers_poller_status() {
        let poller = DetTid::from_raw(1);
        let mut queue = RunQueue::new(SchedHeuristic::None, 0, 0.0);
        queue.push_poller(poller, DEFAULT_PRIORITY, 30);
        assert_eq!(
            queue.restore_poller_priority(poller),
            PollerRestore::Restored
        );
        assert!(queue.only_pollers());
        queue.do_poll_upgrade();
        assert!(
            !queue.only_pollers(),
            "upgraded like every backed-off poller"
        );
    }

    #[test]
    fn only_first_entry_heuristics_are_priority_ordered() {
        for (strategy, ordered) in [
            (SchedHeuristic::None, true),
            (SchedHeuristic::ConnectBind, true),
            (SchedHeuristic::Random, false),
            (SchedHeuristic::StickyRandom, false),
        ] {
            assert_eq!(
                RunQueue::new(strategy, 0, 0.5).is_priority_ordered(),
                ordered,
                "{strategy:?}"
            );
        }
    }

    #[test]
    fn transport_suspend_restore_preserves_all_queue_state() {
        for strategy in [
            SchedHeuristic::None,
            SchedHeuristic::Random,
            SchedHeuristic::StickyRandom,
        ] {
            let tid = DetTid::from_raw(1);
            let peer = DetTid::from_raw(2);
            let mut queue = RunQueue::new(strategy, 391, 0.5);
            queue.push_yielded(tid, DEFAULT_PRIORITY);
            queue.push_back(peer, DEFAULT_PRIORITY);
            let key = *queue
                .queue
                .iter()
                .find(|(_, value)| value.tid == tid)
                .unwrap()
                .0;
            queue.queue.get_mut(&key).unwrap().poll_upgrade = Some(DEFAULT_PRIORITY);
            queue.sticky_random_selection = Some(tid);
            let before = format!("{queue:?}");
            let saved = queue.suspend(tid, DEFAULT_PRIORITY).unwrap();
            assert_eq!(queue.yielded_skip, Some(tid));
            assert_eq!(queue.sticky_random_selection, Some(tid));
            queue.restore(saved, DEFAULT_PRIORITY);
            assert_eq!(format!("{queue:?}"), before, "{strategy:?}");
        }
    }

    #[test]
    fn observation_restore_keeps_real_selection_progress_and_new_priority() {
        for strategy in [
            SchedHeuristic::None,
            SchedHeuristic::Random,
            SchedHeuristic::StickyRandom,
        ] {
            let tid = DetTid::from_raw(1);
            let peer = DetTid::from_raw(2);
            let mut queue = RunQueue::new(strategy, 391, 0.5);
            queue.push_yielded(tid, DEFAULT_PRIORITY);
            queue.push_back(peer, DEFAULT_PRIORITY);
            let key = *queue
                .queue
                .iter()
                .find(|(_, value)| value.tid == tid)
                .unwrap()
                .0;
            queue.queue.get_mut(&key).unwrap().poll_upgrade = Some(DEFAULT_PRIORITY);
            let saved = queue.suspend(tid, DEFAULT_PRIORITY).unwrap();
            assert_eq!(queue.tentative_pop_next(), Some(peer));
            assert_eq!(queue.commit_tentative_pop_completed_turn(), peer);
            assert_eq!(queue.yielded_skip, None);
            queue.push_back(peer, DEFAULT_PRIORITY);
            let random_after_turn = format!("{:?}", queue.prng);
            let sticky_after_turn = queue.sticky_random_selection;
            let turns_after_turn = (queue.last_back_turn, queue.last_front_turn);
            queue.restore(saved, DEFAULT_PRIORITY + 3);
            let (restored, value) = queue
                .queue
                .iter()
                .find(|(_, value)| value.tid == tid)
                .unwrap();
            assert_eq!(restored.turn, key.turn);
            assert_eq!(restored.priority, DEFAULT_PRIORITY + 3);
            assert_eq!(value.poll_upgrade, Some(DEFAULT_PRIORITY + 3));
            assert_eq!(queue.yielded_skip, None);
            assert_eq!(queue.sticky_random_selection, sticky_after_turn);
            assert_eq!(
                (queue.last_back_turn, queue.last_front_turn),
                turns_after_turn
            );
            assert_eq!(format!("{:?}", queue.prng), random_after_turn);
        }
    }

    #[test]
    fn yielded_thread_cedes_exactly_one_turn_under_every_heuristic() {
        for strategy in [
            SchedHeuristic::None,
            SchedHeuristic::ConnectBind,
            SchedHeuristic::Random,
            SchedHeuristic::StickyRandom,
        ] {
            let yielding = DetTid::from_raw(1);
            let peer = DetTid::from_raw(2);
            let mut queue = RunQueue::new(strategy, 0, 1.0);

            queue.push_back(yielding, DEFAULT_PRIORITY - 1);
            assert_eq!(queue.tentative_pop_next(), Some(yielding));
            assert_eq!(queue.commit_tentative_pop(), yielding);

            queue.push_yielded(yielding, DEFAULT_PRIORITY - 1);
            queue.push_back(peer, LAST_PRIORITY);
            assert_eq!(queue.tentative_pop_next(), Some(peer), "{strategy:?}");
            assert_eq!(
                queue.commit_tentative_pop_completed_turn(),
                peer,
                "{strategy:?}"
            );

            assert_eq!(queue.yielded_skip, None, "{strategy:?}");
            let restored_priority = queue
                .queue
                .iter()
                .find(|(_key, value)| value.tid == yielding)
                .map(|(key, _value)| key.priority);
            assert_eq!(
                restored_priority,
                Some(DEFAULT_PRIORITY - 1),
                "{strategy:?}"
            );

            if matches!(strategy, SchedHeuristic::None | SchedHeuristic::ConnectBind) {
                queue.push_back(peer, LAST_PRIORITY);
                assert_eq!(queue.tentative_pop_next(), Some(yielding), "{strategy:?}");
            }
        }
    }

    #[test]
    fn exact_selection_bypasses_priority_and_heuristic() {
        for strategy in [
            SchedHeuristic::None,
            SchedHeuristic::ConnectBind,
            SchedHeuristic::Random,
            SchedHeuristic::StickyRandom,
        ] {
            let higher_priority = DetTid::from_raw(1);
            let selected = DetTid::from_raw(2);
            let mut queue = RunQueue::new(strategy, 0, 1.0);
            queue.push_back(higher_priority, FIRST_PRIORITY);
            queue.push_back(selected, LAST_PRIORITY);

            assert_eq!(queue.tentative_pop_tid(selected), Some(selected));
            assert_eq!(queue.commit_tentative_pop(), selected);
            assert!(queue.contains_tid(higher_priority));
            assert!(!queue.contains_tid(selected));
        }
    }

    #[test]
    fn scheduler_only_commit_does_not_consume_yield_exclusion() {
        let yielding = DetTid::from_raw(1);
        let peer = DetTid::from_raw(2);
        let mut queue = RunQueue::default();

        queue.push_back(yielding, DEFAULT_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(yielding));
        assert_eq!(queue.commit_tentative_pop(), yielding);

        queue.push_yielded(yielding, DEFAULT_PRIORITY);
        queue.push_back(peer, DEFAULT_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(peer));
        assert_eq!(queue.commit_tentative_pop(), peer);

        queue.push_back(peer, DEFAULT_PRIORITY);
        assert_eq!(queue.yielded_skip, Some(yielding));
        assert_eq!(queue.tentative_pop_next(), Some(peer));
        assert_eq!(queue.commit_tentative_pop_completed_turn(), peer);
        assert_eq!(queue.yielded_skip, None);
    }

    /// Pop the next selection and commit it as a completed turn.
    fn take_turn(queue: &mut RunQueue) -> DetTid {
        queue.tentative_pop_next().expect("a runnable thread");
        queue.commit_tentative_pop_completed_turn()
    }

    /// Give `tid` a chaos `sched_yield` turn: commit it and requeue it into
    /// the yield epoch at `priority`.
    fn yield_turn(queue: &mut RunQueue, tid: DetTid, priority: Priority) {
        assert_eq!(queue.tentative_pop_tid(tid), Some(tid));
        assert_eq!(queue.commit_tentative_pop_yield(), tid);
        queue.push_yielded_into_epoch(tid, priority);
    }

    /// Yield `tid` until it joins the yield epoch: its plain yields, then
    /// the one that joins. Another thread must be queued.
    fn yield_into_epoch(queue: &mut RunQueue, tid: DetTid, priority: Priority) {
        for _ in 0..=PLAIN_YIELDS_BEFORE_EPOCH {
            yield_turn(queue, tid, priority);
        }
        assert!(
            queue
                .yield_epoch
                .as_ref()
                .is_some_and(|epoch| epoch.contains(&tid))
        );
    }

    /// The first chaos yield that passes another runnable thread over is a
    /// plain requeue, so the yielder in the earlier band runs again; the next
    /// one joins the epoch, and the other thread runs.
    #[test]
    fn a_yielder_is_reselected_once_before_it_joins_the_epoch() {
        let yielder = DetTid::from_raw(1);
        let awaited = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, FIRST_PRIORITY);
        queue.push_back(awaited, LAST_PRIORITY);

        yield_turn(&mut queue, yielder, FIRST_PRIORITY);
        assert_eq!(queue.yield_epoch, None);
        assert_eq!(queue.tentative_pop_next(), Some(yielder));
        queue.undo_tentative_pop();
        yield_turn(&mut queue, yielder, FIRST_PRIORITY);
        assert_eq!(take_turn(&mut queue), awaited);
        assert!(queue.yield_streaks.is_empty());
    }

    /// Consecutive yields made with no other thread queued build no streak,
    /// so when a thread becomes runnable (a futex waiter woken for a signal,
    /// say) the spinner's next yield is still a plain one and it may run
    /// first. The yields follow one another with no other turn between them,
    /// so nothing else can have emptied the streak (review round 3 of
    /// https://github.com/rrnewton/hermit/pull/4080).
    #[test]
    fn consecutive_lone_yields_build_no_streak() {
        let spinner = DetTid::from_raw(1);
        let woken = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(spinner, FIRST_PRIORITY);
        for _ in 0..5 {
            yield_turn(&mut queue, spinner, FIRST_PRIORITY);
        }
        assert!(queue.yield_streaks.is_empty());
        assert_eq!(queue.yield_epoch, None);
        queue.push_back(woken, LAST_PRIORITY);
        yield_turn(&mut queue, spinner, FIRST_PRIORITY);
        assert_eq!(queue.yield_epoch, None);
        assert_eq!(queue.tentative_pop_next(), Some(spinner));
    }

    /// A thread left as the only one outside an epoch of yielders joins it
    /// by yielding, so the epoch covers the queue and ends, instead of that
    /// thread spinning ahead of the others forever.
    #[test]
    fn the_last_thread_outside_an_epoch_joins_it_by_yielding() {
        let first = DetTid::from_raw(1);
        let last = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(first, FIRST_PRIORITY);
        queue.push_back(last, LAST_PRIORITY);

        yield_into_epoch(&mut queue, first, FIRST_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(last));
        queue.undo_tentative_pop();
        yield_into_epoch(&mut queue, last, LAST_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(first));
        assert_eq!(queue.yield_epoch, None);
    }

    /// Two threads that keep yielding cannot take turns ahead of a third in a
    /// later band, as a one-turn exclusion lets them
    /// (https://github.com/rrnewton/hermit/issues/4068).
    #[test]
    fn two_yielders_cannot_shut_out_a_thread_in_a_later_band() {
        let first = DetTid::from_raw(1);
        let second = DetTid::from_raw(2);
        let awaited = DetTid::from_raw(3);
        let mut queue = RunQueue::default();
        queue.push_back(first, FIRST_PRIORITY);
        queue.push_back(second, FIRST_PRIORITY + 1);
        queue.push_back(awaited, LAST_PRIORITY);

        yield_into_epoch(&mut queue, first, FIRST_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(second));
        queue.undo_tentative_pop();
        yield_into_epoch(&mut queue, second, FIRST_PRIORITY + 1);
        assert_eq!(take_turn(&mut queue), awaited);
    }

    /// The exclusion lasts only until a thread that did not yield has had a
    /// completed turn; then the bands decide again, so a yielder is never held
    /// behind later turns of a thread in an earlier band.
    #[test]
    fn a_turn_by_a_thread_that_did_not_yield_ends_the_yield_epoch() {
        let yielder = DetTid::from_raw(1);
        let early = DetTid::from_raw(2);
        let late = DetTid::from_raw(3);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, FIRST_PRIORITY + 1);
        queue.push_back(early, FIRST_PRIORITY);
        queue.push_back(late, LAST_PRIORITY);

        yield_into_epoch(&mut queue, yielder, FIRST_PRIORITY + 1);
        assert_eq!(take_turn(&mut queue), early);
        assert_eq!(queue.yield_epoch, None);
        queue.push_back(early, FIRST_PRIORITY);
        assert_eq!(take_turn(&mut queue), early);
        queue.push_back(early, FIRST_PRIORITY);
        // A bare queue commit, as a poller's non-committing backoff makes,
        // keeps the epoch; the scheduler ends it explicitly where a thread that
        // did not yield ran (a priority change point, a background grant).
        yield_into_epoch(&mut queue, yielder, FIRST_PRIORITY + 1);
        queue.tentative_pop_next();
        assert_eq!(queue.commit_tentative_pop(), early);
        queue.push_back(early, FIRST_PRIORITY);
        assert!(
            queue
                .yield_epoch
                .as_ref()
                .is_some_and(|epoch| epoch.contains(&yielder))
        );
        queue.end_yield_epoch();
        assert_eq!(queue.yield_epoch, None);
        assert!(queue.yield_streaks.is_empty());
        assert_eq!(
            queue.tids().copied().collect::<Vec<_>>(),
            [early, yielder, late]
        );
    }

    /// An epoch that covers every queued thread is over, so yielders that
    /// wait on each other alternate instead of the first one running forever.
    #[test]
    fn a_yield_epoch_covering_every_queued_thread_ends() {
        let first = DetTid::from_raw(1);
        let second = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(first, FIRST_PRIORITY);
        queue.push_back(second, LAST_PRIORITY);

        yield_into_epoch(&mut queue, first, FIRST_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(second));
        queue.undo_tentative_pop();
        yield_into_epoch(&mut queue, second, LAST_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(first));
        assert_eq!(queue.yield_epoch, None);
        queue.undo_tentative_pop();
        yield_into_epoch(&mut queue, first, FIRST_PRIORITY);
        assert_eq!(queue.tentative_pop_next(), Some(second));
    }

    /// A yielder keeps its queue key: it does not join the last band, so it
    /// is not counted as a poller by the deterministic-work-first gates.
    #[test]
    fn a_yield_epoch_keeps_the_yielders_band_and_poller_status() {
        let yielder = DetTid::from_raw(1);
        let awaited = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, FIRST_PRIORITY);
        queue.push_back(awaited, LAST_PRIORITY);

        yield_into_epoch(&mut queue, yielder, FIRST_PRIORITY);
        assert!(!queue.only_pollers());
        assert!(queue.has_runnable_besides(awaited));
        assert_eq!(
            queue.tids().copied().collect::<Vec<_>>(),
            [yielder, awaited]
        );
        assert_eq!(take_turn(&mut queue), awaited);
    }

    /// A thread that leaves the queue leaves the yield epoch, so a yielder
    /// readmitted at once, as a signal's forced unblock does, is not skipped.
    #[test]
    fn leaving_the_queue_leaves_the_yield_epoch() {
        let yielder = DetTid::from_raw(1);
        let other = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, FIRST_PRIORITY);
        queue.push_back(other, LAST_PRIORITY);

        yield_into_epoch(&mut queue, yielder, FIRST_PRIORITY);
        assert!(queue.remove_tid(yielder));
        queue.push_front(yielder, FIRST_PRIORITY);
        assert_eq!(take_turn(&mut queue), yielder);
    }

    /// A poller backed off all the way to the last band does not hold a yield
    /// epoch open: its retries do not commit, so a yielder it waits on would
    /// otherwise never run. A thread that is merely in the last band, as
    /// `--chaos-target-races` places half of all new threads, does hold it.
    #[test]
    fn only_a_fully_backed_off_poller_releases_a_yield_epoch() {
        let yielder = DetTid::from_raw(1);
        let other = DetTid::from_raw(2);
        for poller in [false, true] {
            let mut queue = RunQueue::default();
            queue.push_back(yielder, FIRST_PRIORITY);
            if poller {
                queue.push_poller(other, DEFAULT_PRIORITY, 20);
            } else {
                queue.push_back(other, LAST_PRIORITY);
            }
            yield_into_epoch(&mut queue, yielder, FIRST_PRIORITY);
            let expected = if poller { yielder } else { other };
            assert_eq!(
                queue.tentative_pop_next(),
                Some(expected),
                "poller={poller}"
            );
        }
    }

    /// A thread that leaves the queue loses its streak too, so after it is
    /// readmitted its next yield past another thread is a plain one again.
    #[test]
    fn leaving_the_queue_resets_the_yield_streak() {
        let yielder = DetTid::from_raw(1);
        let other = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, FIRST_PRIORITY);
        queue.push_back(other, LAST_PRIORITY);

        yield_turn(&mut queue, yielder, FIRST_PRIORITY);
        assert!(queue.remove_tid(yielder));
        queue.push_front(yielder, FIRST_PRIORITY);
        yield_turn(&mut queue, yielder, FIRST_PRIORITY);
        assert_eq!(queue.yield_epoch, None);
        assert_eq!(queue.tentative_pop_next(), Some(yielder));
    }

    /// A one-turn skip (a loopback poll's yield) and the yield epoch together
    /// never exclude every queued thread: the skip alone then holds.
    #[test]
    fn a_one_turn_skip_and_a_yield_epoch_never_exclude_everyone() {
        let yielder = DetTid::from_raw(1);
        let poller = DetTid::from_raw(2);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, FIRST_PRIORITY);
        queue.push_back(poller, DEFAULT_PRIORITY);

        yield_into_epoch(&mut queue, yielder, FIRST_PRIORITY);
        assert_eq!(queue.tentative_pop_tid(poller), Some(poller));
        assert_eq!(queue.commit_tentative_pop_yield(), poller);
        queue.push_yielded(poller, DEFAULT_PRIORITY);
        // The epoch holds the yielder; the skip holds the poller; together they
        // would cover the queue, so only the skip applies.
        assert_eq!(queue.tentative_pop_next(), Some(yielder));
    }

    /// A yielder alone in the queue runs again, and the epoch ends.
    #[test]
    fn a_lone_yielder_runs_again() {
        let yielder = DetTid::from_raw(1);
        let mut queue = RunQueue::default();
        queue.push_back(yielder, DEFAULT_PRIORITY);

        yield_turn(&mut queue, yielder, DEFAULT_PRIORITY);
        assert_eq!(take_turn(&mut queue), yielder);
        assert_eq!(queue.yield_epoch, None);
        assert!(queue.yield_streaks.is_empty());
    }

    /// The epoch holds under the random heuristics too: after two yielders,
    /// the third thread runs.
    #[test]
    fn a_yield_epoch_holds_under_the_random_heuristics() {
        for strategy in [SchedHeuristic::Random, SchedHeuristic::StickyRandom] {
            for seed in 0..16 {
                let yielders = [DetTid::from_raw(1), DetTid::from_raw(2)];
                let awaited = DetTid::from_raw(3);
                let mut queue = RunQueue::new(strategy, seed, 1.0);
                for tid in [yielders[0], yielders[1], awaited] {
                    queue.push_back(tid, DEFAULT_PRIORITY);
                }
                for tid in yielders {
                    yield_into_epoch(&mut queue, tid, DEFAULT_PRIORITY);
                }
                assert_eq!(take_turn(&mut queue), awaited, "{strategy:?} seed {seed}");
            }
        }
    }

    #[test]
    fn threads_queued_by_a_yield_are_not_work_a_background_call_waits_behind() {
        let first = DetTid::from_raw(1);
        let second = DetTid::from_raw(2);
        let worker = DetTid::from_raw(3);
        let mut queue = RunQueue::default();

        queue.push_back(first, DEFAULT_PRIORITY);
        queue.push_back(second, DEFAULT_PRIORITY);
        assert!(!queue.only_pollers_or_yielders());

        // Two spinners take turns: each yield replaces the other's
        // `yielded_skip`, and both still count as yielders.
        assert_eq!(queue.tentative_pop_next(), Some(first));
        assert_eq!(queue.commit_tentative_pop_completed_turn(), first);
        queue.push_yielded(first, DEFAULT_PRIORITY);
        assert!(
            !queue.only_pollers_or_yielders(),
            "second has not yielded yet"
        );
        assert_eq!(queue.tentative_pop_next(), Some(second));
        assert_eq!(queue.commit_tentative_pop_completed_turn(), second);
        queue.push_yielded(second, DEFAULT_PRIORITY);
        assert_eq!(queue.yielded_skip, Some(second));
        assert!(!queue.only_pollers());
        assert!(queue.only_pollers_or_yielders());

        // Ordinary work besides them still counts.
        queue.push_back(worker, DEFAULT_PRIORITY);
        assert!(!queue.only_pollers_or_yielders());
        assert!(queue.remove_tid(worker));
        assert!(queue.only_pollers_or_yielders());

        // A yielder that runs again and is queued without yielding is work.
        assert_eq!(queue.tentative_pop_next(), Some(first));
        assert_eq!(queue.commit_tentative_pop_completed_turn(), first);
        queue.push_back(first, DEFAULT_PRIORITY);
        assert!(!queue.only_pollers_or_yielders());
    }

    #[test]
    fn tentative_pop_in_progress_tracks_the_transaction() {
        let a = DetTid::from_raw(1);
        let b = DetTid::from_raw(2);
        let mut queue = RunQueue::default();

        // No selection: safe to push.
        assert!(!queue.tentative_pop_in_progress());
        queue.push_back(a, DEFAULT_PRIORITY);
        queue.push_back(b, DEFAULT_PRIORITY);
        assert!(!queue.tentative_pop_in_progress());

        // Peeking a tentative selection opens the transaction; this is exactly
        // the window in which a concurrent handler must defer its admission
        // rather than push (a push here trips the tentative-selection guard).
        assert_eq!(queue.tentative_pop_next(), Some(a));
        assert!(queue.tentative_pop_in_progress());

        // Committing closes it again.
        assert_eq!(queue.commit_tentative_pop(), a);
        assert!(!queue.tentative_pop_in_progress());

        // The exact-selection form and undo path behave the same way.
        assert_eq!(queue.tentative_pop_tid(b), Some(b));
        assert!(queue.tentative_pop_in_progress());
        queue.undo_tentative_pop();
        assert!(!queue.tentative_pop_in_progress());
    }

    #[test]
    fn removal_clears_per_incarnation_selection_state_before_tid_reuse() {
        let tid = DetTid::from_raw(7);
        let mut queue = RunQueue::new(SchedHeuristic::StickyRandom, 0x5107, 1.0);
        queue.push_back(tid, DEFAULT_PRIORITY);

        assert_eq!(queue.tentative_pop_next(), Some(tid));
        queue.undo_tentative_pop();
        assert_eq!(queue.sticky_random_selection, Some(tid));
        // Model an earlier explicit yield by the old incarnation. Both caches
        // are keyed only by raw TID and therefore must be cleared together.
        queue.yielded_skip = Some(tid);

        assert!(queue.remove_tid(tid));
        assert_eq!(queue.sticky_random_selection, None);
        assert_eq!(queue.yielded_skip, None);

        queue.push_back(tid, DEFAULT_PRIORITY);
        assert_eq!(queue.sticky_random_selection, None);
        assert_eq!(queue.yielded_skip, None);
    }
}
