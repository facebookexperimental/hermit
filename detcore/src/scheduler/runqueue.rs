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
//! compared in that order. Round-robin orders monotinically increase across
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
use std::fmt;
use std::fmt::Display;

use rand::Rng;
use rand::SeedableRng;
use rand::distributions::uniform::SampleUniform;
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

/// After queueing this many new tasks, perform a "poll upgrade," in which
/// we upgrade all outstanding polling tasks their original priority levels,
/// temporarily negating backoff behavior.
const POLLING_UPGRADE_INTERVAL: u64 = 200;

#[derive(Debug, Copy, Clone)]
struct QueueValue {
    tid: DetTid,
    /// Upgrade to this priority during polling upgrades
    poll_upgrade: Option<Priority>,
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
            &self.queue.len(),
            &self.last_back_turn,
            &self.last_front_turn,
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

    // Return the numerically least Priority value in the run_queue, or None if the queue is empty.
    pub fn first_priority(&self) -> Option<Priority> {
        let (k, _) = self.queue.first_key_value()?;
        Some(k.priority)
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
        let qval = QueueValue { tid, poll_upgrade };
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
        !kept_all
    }

    // Helper function for logging purposes.
    fn random_range<T>(&mut self, start: T, end: T) -> T
    where
        T: SampleUniform + Display + PartialOrd + Copy,
    {
        let r = self.prng.gen_range(start..end);
        detlog!("SCHEDRAND: [{},{}) => {}", start, end, r);
        r
    }

    /// Begin, but do not complete, a pop_transaction.  This can be committed or undone later.  But
    /// one of those must happen before other modification operations can occur on the RunQueue.
    ///
    /// Postcondition: if return a `Some` value, the RunQueue enters a *locked* state where
    /// commit or undo must happen before any other mutations to the structure.
    pub fn tentative_pop_next(&mut self) -> Option<DetTid> {
        self.tentative_selection = match self.sched_strategy {
            SchedHeuristic::None | SchedHeuristic::ConnectBind => {
                self.queue.first_key_value().map(|(_k, v)| v.tid)
            }
            SchedHeuristic::Random => {
                if self.queue.is_empty() {
                    return None;
                }

                // If there is not Tid picked from a previous operation, let's pick one now.
                if self.tentative_selection.is_none() {
                    let random_idx = self.random_range(0, self.queue.len());
                    self.tentative_selection =
                        self.queue.iter().nth(random_idx).map(|(_k, v)| v.tid);
                };

                self.tentative_selection
            }
            SchedHeuristic::StickyRandom => {
                if self.queue.is_empty() {
                    return None;
                }

                if self.sticky_random_selection.is_none()
                    || !self.contains_tid(self.sticky_random_selection.unwrap())
                {
                    let random_idx = self.random_range(0, self.queue.len());
                    self.sticky_random_selection =
                        self.queue.iter().nth(random_idx).map(|(_k, v)| v.tid);
                }

                self.sticky_random_selection
            }
        };

        self.tentative_selection
    }

    /// Complete the tentative pop operation, readying the RunQueue for future operations.  This
    /// operation is only permissible when the queue is locked, i.e. the tentative_pop has
    /// previously returned `Some`.
    pub fn commit_tentative_pop(&mut self) -> DetTid {
        // Check that queue is locked and unlock it.
        let tentative_selection = self
            .tentative_selection
            .take()
            .expect("tentative_pop to already returned a `Some`");

        let ret = match self.sched_strategy {
            SchedHeuristic::None | SchedHeuristic::ConnectBind => {
                self.queue.first_entry().map(|e| e.remove().tid)
            }
            SchedHeuristic::Random => {
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
        .expect("to always return a DetTid");
        // The above should always return a DetTid as we peeked right before.
        // If this invariant is violated, then it's a bug, or the queue is modified
        // between the peek and tentative_pop and commit_tentative_pop.
        debug_assert!(ret == tentative_selection);
        ret
    }

    /// Forget the tentative pop as though it never happened.
    pub fn undo_tentative_pop(&mut self) {
        assert!(self.tentative_selection.is_some());
        self.tentative_selection = None;
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
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new(SchedHeuristic::None, 0, 0.0)
    }
}
