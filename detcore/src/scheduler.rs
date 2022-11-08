// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Deterministic scheduling algorithm.

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

use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
pub use runqueue::entropy_to_priority;
use runqueue::PrioritizedOrder;
pub use runqueue::Priority;
use runqueue::RunQueue;
pub use runqueue::DEFAULT_PRIORITY;
use runqueue::LAST_PRIORITY;
use runqueue::REPLAY_DEFERRED_PRIORITY;
use runqueue::REPLAY_FOREGROUND_PRIORITY;
use serde::Deserialize;
use serde::Serialize;
use timed_waiters::TimedEvents;
use tracing::debug;
use tracing::enabled;
use tracing::info;
use tracing::trace;
use tracing::Level;

use crate::config::Config;
use crate::detlog_debug;
use crate::ivar::Ivar;
use crate::preemptions::read_trace;
use crate::preemptions::PreemptionWriter;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::FutexID;
use crate::types::GlobalTime;
use crate::types::LogicalTime;
use crate::types::Op;
use crate::types::SchedEvent;
use crate::types::SyscallPhase;

/// Unique identifier for an action.
pub type ActionID = u64;

/// A representation of side effects that are happening, or could be happening, right now
/// in the background.
#[derive(Debug, Clone)]
pub struct Action {
    /// Id for the action
    pub action_id: ActionID,

    /// The action's side effects are completed.
    pub completion: Ivar<()>,

    /// Which action gets the lock after me.
    pub successors: HashMap<ResourceID, ActionID>,
}

/// The response from the scheduler that wakes back up a guest thread after a request.
#[derive(Debug, Clone)]
pub enum SchedResponse {
    /// Keep running.
    Go(Option<SchedValue>),
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
    TimeOut,
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

/// Request for resources when the thread next parks.
/// OR the thread might "park" because it's really exited.
pub type SchedRequest = Result<Resources, ThreadExited>;

/// Unit value to signal that the thread has exited.
// TODO: could put an exit status here.
#[derive(Debug, Clone)]
pub struct ThreadExited;

/// This thread ID is waiting on a futex.
pub type FutexWaiter = (DetTid, Ivar<SchedResponse>);

/// Actions that are blocked on another internal action of the guest, such as a pipe communication,
/// or are blocked on external conditions such as a network request.  These cannot consume a logical
/// turn until a matching unblocking action is ready.
///
/// This structure will NOT include blocking operations that are implemented via polling.
/// See NOTE [Blocking Syscalls via Internal Polling] in this folder.
#[derive(Debug, Clone, Default)]
pub struct BlockedPool {
    /// BLOCKED futex transactions, waiting for wakers.  Multiple threads may be blocked n
    /// the same futex.
    ///
    /// INVARIANT: because Futexes aren't currently modeled with `ResourceID`, a thread
    /// waiting on a futex will have a request filled in `next_turns` but for zero resources.
    ///
    /// TODO: the entries of this hashmap are never reclaimed (but they could be).
    pub futex_waiters: HashMap<FutexID, Vec<FutexWaiter>>,

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
    /// waiting for permission to resume.  The request will stay empty while the thread is
    /// doing the blocking action.  This is different than the normal relationship
    pub external_io_blockers: BTreeSet<DetTid>,
}

impl BlockedPool {
    /// Returns true if there are NO blocked threads waiting outside the run-queue.
    fn is_empty(&self) -> bool {
        self.no_futex_waiters()
            && self.timed_waiters.is_empty()
            && self.external_io_blockers.is_empty()
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
fn assert_continue_request(req: &Resources) {
    assert_eq!(req.resources.len(), 1);
    let rsrc = req.resources.iter().next().unwrap().0;
    assert_eq!(rsrc, &ResourceID::BlockedExternalContinue);
}

/// The state for the deterministic scheduler.
#[derive(Debug, Default)]
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
    pub bg_action_pool: HashMap<ActionID, Action>,

    /// The logical, global time consumed by actions that have been committed already.
    pub committed_time: LogicalTime,

    /// INVARIANT: Thread IDs in `blocked` are absent from `run_queue`.
    pub blocked: BlockedPool,

    /// Ac table of "locks held": which action is using which resources.
    /// A given resource can be held by at most one action at a given time.
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

    /// A record of which preemptions occured on each thread.  Only used IF `--record-preemptions`
    /// was specified in the Config, otherwise this remains empty.
    pub preemption_writer: Option<PreemptionWriter>,

    /// A cursor that holds our place in the global total order of events being replayed.
    pub replay_cursor: Option<Peekable<IntoIter<SchedEvent>>>,

    /// Keep track of how many events we have replayed.  The current value is the event number of
    /// the NEXT event to replay.
    pub traced_event_count: u64,

    /// Like `traced_event_count` but for record_event.  These should match if we're both replaying
    /// and recording at the same time.
    pub recorded_event_count: u64,

    /// A copy of the `Config::stacktrace_event` vector.  This is MUTABLE,
    /// because we pop events off as we handle them.  The u64 is an index into
    /// the (original) replay_cursor trace.
    pub stacktrace_events: Option<StacktraceEventsIter>,

    /// A cached copy of the same (immutable) field in Config.
    stop_after_turn: Option<u64>,
    /// A cached copy of the same (immutable) field in Config.
    stop_after_iter: Option<u64>,
    /// A cached copy of the same (immutable) field in Config.
    recordreplay_modes: bool,
    /// A cached copy of the same (immutable) field in Config.
    die_on_desync: bool,
    /// A cached copy of the same (immutable) field in Config.
    replay_exhausted_panic: bool,
}

type StacktraceEventsIter = Peekable<IntoIter<(u64, Option<PathBuf>)>>;

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
}

use pretty::Doc;
use pretty::RcDoc;
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

    /// Print out some information at the end of a run.
    pub fn final_report(&self) -> String {
        let mut buf = String::new();
        writeln!(buf, "Final thread-tree was: {}", self).unwrap();
        writeln!(
            buf,
            "There were {} group leaders of {} thread(s) total.",
            self.thread_group_leaders.len(),
            self.size(),
        )
        .unwrap();
        buf
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

    async fn further(&mut self) {
        self.count += 1;
        const YIELDS_FIRST: u64 = 10;
        if self.count <= YIELDS_FIRST {
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

pub(crate) async fn sched_loop(sched: Arc<Mutex<Scheduler>>, timer: Arc<Mutex<GlobalTime>>) {
    info!("[scheduler] daemon task starting up, waiting for guest thread start..");
    let (iv, stop_after_iter) = {
        // Block until queue is populated.
        let sched = sched.lock().unwrap();
        (sched.started_up.clone(), sched.stop_after_iter)
    };
    iv.get().await;
    info!("[scheduler] guest in queue, scheduler proceeding..",);
    let mut iter: u64 = 0;
    // We keep track of whether the last turn was a SKIP:
    let mut last_res = Err(SkipTurn);
    let mut backoff = Backoff::new();

    loop {
        // TODO (T137183027, T137184765): as part of the current strategy for blocking IO ops (see
        // SPINNING below), we need to make sure that other threads can progress so we don't
        // busy-wait too tightly.
        if last_res.is_err() {
            backoff.further().await;
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
            if sched.run_queue.is_empty() && sched.blocked.is_empty() {
                info!("[scheduler] run queue empty, exiting sched_loop.");
                return;
            } else if let Some(stop) = sched.stop_after_turn {
                if sched.turn > stop {
                    tracing::warn!(
                        "[scheduler] Early exit during turn {} due to --stop-after-turn.  Summary:\n\n{}",
                        sched.turn,
                        sched.full_summary()
                    );
                    immediate_fatal_exit(); // We don't want a backtrace of this thread.
                }
            }
        }

        // Otherwise we trust the turn function to either choose a runnable thread or wait
        // until something blocked is ready to run again.
        last_res = do_a_turn_blocking(sched.clone(), timer.clone(), &last_res).await;
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
            // TODO: check status of next_dtid -- in runqueue (or not)?
            return Err(SkipTurn);
        }
        Ok(r) => r,
    };
    trace!("[sched-daemon] daemon woke up on {}...", &req);

    // Since the scheduler is asynchronous, we need to check our assumptions.  Polling is
    // sufficient here because the thread cannot be racing with us to exit since we know
    // it is *already* parked.
    if !sched.lock().unwrap().next_turns.contains_key(&next_dtid) {
        info!(
            "[sched-daemon] thread {} exited, skipping over...",
            &next_dtid
        );
    } else {
        let mut mg = sched.lock().unwrap();

        // The logical COMMIT point for the turn is during step4:
        mg.step4_resource_block(next_dtid, &rsrcs, &resp)?;
        mg.step5_guest_unblock(next_dtid, &rsrcs, &resp)?;
        mg.step6_reenquue(next_dtid);
        if let Some(call) = rsrcs.as_exit_syscall() {
            mg.step7_simulate_exit_posthook(next_dtid, call, &global_time);
        }
    }
    Ok(rsrcs)
}

// A futex request contains only one resource request, for FutexWait.
fn assert_futex_request(nextturn: &ThreadNextTurn) {
    match nextturn.req.try_read() {
        Some(Ok(req)) => {
            if !(req.resources.get(&ResourceID::FutexWait).is_some() && req.resources.len() == 1) {
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
        Some(Ok(req)) => {
            if req.resources.get(&ResourceID::FutexWait).is_some() {
                assert!(
                    req.resources.len() == 1,
                    "internal futex resource request invariant"
                );
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Is a desync considered fatal, or just kinda bad.
/// RCB divergence is the later, mismatched syscalls are currently the former.
fn is_hard_desync(observed: &SchedEvent, expected: &SchedEvent) -> bool {
    let mut strip1 = observed.clone();
    let mut strip2 = expected.clone();
    strip1.end_time = None;
    strip2.end_time = None;
    strip1.count = 0;
    strip2.count = 0;
    strip1 != strip2
}

fn compare_desync(observed: &SchedEvent, expected: &SchedEvent) -> String {
    if *observed == *expected {
        "MATCHED".to_string()
    } else if observed.op != expected.op {
        "FULL-OP-DESYNC".to_string()
    } else {
        let mut msg = "DESYNC".to_string();
        if observed.start_rip != expected.start_rip || observed.end_rip != expected.end_rip {
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
    /// Should we print the stacktrace in the guest, as per --stacktrace-event
    pub print_stack: bool,
}

impl Scheduler {
    /// Create a new scheduler based on the configuration.
    pub fn new(cfg: &Config) -> Self {
        // Just like chaos_prng, use the default seed if this internal
        // scheduler-seed isn't specifically provided by the user:
        let sched_seed = cfg.sched_seed.unwrap_or(cfg.seed);
        Self {
            preemption_writer: if cfg.record_preemptions {
                Some(PreemptionWriter::new(cfg.record_preemptions_to.clone()))
            } else {
                None
            },
            replay_cursor: match &cfg.replay_schedule_from {
                Some(path) => {
                    trace!("Scheduler loading trace from path {}", path.display());
                    let vec = read_trace(path);
                    trace!("Trace loaded, length {}", vec.len());
                    Some(vec.into_iter().peekable())
                }
                None => None,
            },
            traced_event_count: 0,
            recorded_event_count: 0,
            stacktrace_events: if cfg.stacktrace_event.is_empty() {
                None
            } else {
                Some(cfg.stacktrace_event.clone().into_iter().peekable())
            },
            stop_after_turn: cfg.stop_after_turn,
            stop_after_iter: cfg.stop_after_iter,
            recordreplay_modes: cfg.recordreplay_modes,
            run_queue: RunQueue::new(
                cfg.sched_heuristic,
                sched_seed,
                cfg.sched_sticky_random_param,
            ),
            die_on_desync: cfg.die_on_desync,
            replay_exhausted_panic: cfg.replay_exhausted_panic,
            turn: 0,
            next_turns: Default::default(),
            bg_action_pool: Default::default(),
            committed_time: Default::default(),
            blocked: Default::default(),
            resources: Default::default(),
            started_up: Default::default(),
            thread_tree: Default::default(),
            priorities: Default::default(),
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
        req.put(Ok(rs));
    }

    /// Poll the resource request and *if* it is not currently observed to be full, return
    /// the IVar that *will* contain it in the future.
    fn check_request(&self, det_tid: &DetTid) -> Option<Ivar<SchedRequest>> {
        let nextturn = self.next_turns.get(det_tid).unwrap_or_else(|| {
            panic!(
                "[check_request] internal error: dettid {} queued but missing entry in next_turns",
                &det_tid
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
        self.run_queue.tids().find_map(|dt| self.check_request(dt))
    }

    /// Try to pop the next event from the sorted list of stacktrace_events, if it matches the given
    /// index.  This is idempotent, because subsequent attempts will just fizzle.
    fn try_pop_stacktrace_event(&mut self, current_ix: u64) -> bool {
        let mut result = false;
        if let Some(iter) = &mut self.stacktrace_events {
            if let Some((next_ix, _)) = iter.peek() {
                if *next_ix == current_ix {
                    eprintln!(
                        "\nPrinting stack trace for scheduled event #{}:",
                        current_ix
                    );
                    let _ = iter.next();
                    result = true;
                }
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
        debug_assert!(self.replay_cursor.is_some());

        let mytid = observed.dettid;
        let time = observed.end_time.expect("timestamps required for now");

        let current_ix = self.traced_event_count;
        self.traced_event_count += 1;
        let print_stack = self.try_pop_stacktrace_event(current_ix);

        if let Some(expected) = self
            .replay_cursor
            .as_mut()
            .expect("replay iterator set while in replay mode")
            .next()
        {
            debug!(
                "[detcore, dtid {}] {}: Ran event #{} {:?}, current replay event: {:?}",
                mytid,
                compare_desync(observed, &expected),
                current_ix,
                observed,
                expected,
            );
            trace!(
                "[detcore, dtid {}] NEXT event to replay {:?}",
                mytid,
                self.replay_cursor.as_mut().unwrap().peek()
            );

            if is_hard_desync(observed, &expected) && self.die_on_desync {
                eprintln!("Replay mode desynchronized from trace, bailing out.");
                immediate_fatal_exit();
            }

            let keep_running = if let Some(next_ev) = self.replay_cursor.as_mut().unwrap().peek() {
                let next_tid = next_ev.dettid;
                if next_tid != observed.dettid {
                    let is_prehook = matches!(observed.op, Op::Syscall(_, SyscallPhase::Prehook));
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
                    self.requeue_with_new_priority(mytid, REPLAY_DEFERRED_PRIORITY);
                    self.requeue_with_new_priority(next_tid, REPLAY_FOREGROUND_PRIORITY);
                    // The *downgrading* of the current thread will be handled by the caller if the
                    // context switch is *now*.  If the last traced event on this thread is instead
                    // a prehook, well we don't deschedule the current thread quite yet.  Rather, we
                    // let the thread plow ahead, and actually block on the syscall, which will have
                    // the effect of descheduling the therad anyway.  After that, the priorities
                    // be set so as to make sure the correct thread (next_tid) runs.
                    is_prehook
                } else {
                    true // We're still running the next event.
                }
            } else {
                true // We're the very last event.  Nothing to do.
            };
            ConsumeResult {
                keep_running,
                print_stack,
            }
        } else {
            if self.replay_exhausted_panic {
                eprintln!(
                    "[detcore, dtid {}] Replay trace ran out, stopping at unknown event {:?}",
                    &mytid, observed
                );
                immediate_fatal_exit();
            } else {
                info!(
                    "[detcore, dtid {}] Replay trace ran out, unknown event {:?}",
                    &mytid, observed
                );
            }
            // We're PAST the last event.  Uncharted territory...
            ConsumeResult {
                keep_running: true,
                print_stack: false,
            }
        }
    }

    /// Remove a thread from the deterministic scheduler.  In order to call this, the precondition
    /// is that this thread will execute no further (visible) instructions.
    ///
    /// This is called while the guest is running, not in the middle of a scheduler turn.
    ///
    /// This is IDEMPOTENT, and it may indeed be called twice, both to proactively remove a thread,
    /// and then reactively in response to an exit hook.
    pub fn logically_kill_thread(&mut self, dtid: &DetTid, detpid: &DetPid) {
        info!(
            "logically_kill: Scheduler removing all knowledge of [det]tid {} in pid {}..",
            dtid, detpid
        );
        for vec in &mut self.blocked.futex_waiters.values_mut() {
            vec.retain(|(dt2, _)| dt2 != dtid);
        }
        self.blocked.timed_waiters.remove(*dtid);
        let _ = self.blocked.external_io_blockers.remove(dtid);
        let _ = self.run_queue.remove_tid(*dtid);
        let _ = self.priorities.remove(dtid);
        match self.next_turns.remove(dtid) {
            None => {
                trace!(
                    "logically_kill_thread: thread already removed from scheduler: {}",
                    &dtid
                );
            }
            Some(nextturn) => {
                // Put in a dummy request to unblock the scheduler that might be
                // waiting for the thread to park.
                //
                // WARNING: this try_put should potentially turn back into a put(), if we can narrow
                // down the exit scenarios and ensure that they happen when the guest is running and
                // has NOT filled its request to the scheduler yet.
                nextturn.req.try_put(Err(ThreadExited));
                self.wake_futex_child_cleartid((*detpid, nextturn.child_tid_addr), *dtid);
            }
        }
    }

    /// Put a Futex waiter to sleep, to be awoken by `wake_futex_waiter`.
    pub fn sleep_futex_waiter(
        &mut self,
        dettid: &DetTid,
        futexid: FutexID,
        maybe_timeout: Option<LogicalTime>,
    ) {
        let nxt = self
            .next_turns
            .get(dettid)
            .expect("Missing next_turns entry");
        let entry: &mut Vec<_> = self
            .blocked
            .futex_waiters
            .entry(futexid)
            .or_insert_with(Vec::new);
        entry.push((*dettid, nxt.resp.clone()));
        // When we park, we use a resource request to signal WHAT we're blocking on.  But this is
        // not quite the same as when an active thread in the runqueue blocks on a resource, because
        // we're not actually waiting on the scheduler giving us the resource.  We're waiting in the
        // futex_waiters pool until a waker comes along.
        let mut rsrc = Resources::new(*dettid);
        rsrc.insert(ResourceID::FutexWait, Permission::R);
        nxt.req.put(Ok(rsrc));
        trace!(
            "[detcore, dtid {}] Waiter blocking on futex {:?}, now {} waiters, on {}",
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
    pub fn wake_futex_waiter(&mut self, (waiterid, waiter_ivar): FutexWaiter) {
        debug_assert!(!self.run_queue.contains_tid(waiterid));

        // If it was registered as a waiter-with-timeout, remove it:
        self.blocked.timed_waiters.remove(waiterid);

        // Put the woken thread back into circulation:
        let pos = self.runqueue_push_back(waiterid);
        trace!(
            "[detcore] Woke one thread, dtid: {}, ivar {:p}, scheduled at position {}",
            &waiterid,
            &waiter_ivar,
            pos,
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

    /// Reschedule all threads blocked on a particular futex.
    /// TODO: support rescheduling exactly K threads.
    pub fn wake_futex_waiters(
        &mut self,
        _waker_dettid: DetTid,
        futexid: FutexID,
        max_to_wake: i32,
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
        let num_woken: usize = std::cmp::min(vec.len(), max_to_wake.try_into().unwrap());
        let to_wake = vec.split_off(vec.len() - num_woken);
        assert_eq!(to_wake.len(), num_woken);
        for waiter in to_wake {
            self.wake_futex_waiter(waiter);
        }
        // Put back what wasn't woken up:
        if !vec.is_empty() {
            let junk = self.blocked.futex_waiters.insert(futexid, vec);
            assert!(junk.unwrap().is_empty());
        }
        num_woken as u64
    }

    /// Simulate the effect of CLONE_CHILD_CLEARTID.
    pub fn wake_futex_child_cleartid(&mut self, futid: FutexID, dettid: DetTid) {
        debug!(
            "simulate CLONE_CHILD_CLEARTID on futex {:?}, wake one",
            futid
        );
        // Wakes only one thread, as per:
        // https://man7.org/linux/man-pages/man2/set_tid_address.2.html
        self.wake_futex_waiters(dettid, futid, 1);
    }

    /// Step: Before we select which thread to run, first we check if some internal data
    /// structure maintenance is necessary, i.e. moving timed events from the waiting pool
    /// to the run queue. It manipulates scheduler data structures accordingly.
    fn step2_process_blocked(
        &mut self,
        global_time: &Arc<Mutex<GlobalTime>>,
    ) -> Result<(), SkipTurn> {
        self.step2b_process_timed(); // May populate run_queue.
        self.step2c_process_io_blockers()?;
        self.step2d_handle_empty_queue(global_time)?;
        Ok(())
    }

    /// Check whether it is time for the *earliest* time-based event to execute INSTEAD of
    /// dispatching from the normal run queue.  Manipulates scheduler data structures
    /// accordingly.
    fn step2b_process_timed(&mut self) {
        if let Some((time_ns, dettid)) = self
            .blocked
            .timed_waiters
            .pop_if_before(self.committed_time)
        {
            self.wake_timed_event(time_ns, dettid)
        }
    }

    fn wake_timed_event(&mut self, time_ns: LogicalTime, dettid: DetTid) {
        if enabled!(Level::TRACE) {
            let nxtturn = self
                .next_turns
                .get_mut(&dettid)
                .expect("internal invariant broken");

            if is_futex_request(nxtturn) {
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

    /// Check on threads that were backgrounded performing external IO.
    fn step2c_process_io_blockers(&mut self) -> Result<(), SkipTurn> {
        if !self.blocked.external_io_blockers.is_empty() {
            // A nondeterministic snapshot of which blocking IO actions are ready right now:
            let ready: Vec<DetTid> = self
                .blocked
                .external_io_blockers
                .iter()
                .filter(|dtid| {
                    let nt = self
                        .next_turns
                        .get(dtid)
                        .expect("internal invariant broken");
                    if let Some(Ok(req)) = nt.req.try_read() {
                        assert_continue_request(&req);
                        true
                    } else {
                        false
                    }
                })
                .cloned()
                .collect();
            debug!(
                "Nondeterministic status of blocking IO: out of {}, completed on {}, dtids: {:?}",
                self.blocked.external_io_blockers.len(),
                ready.len(),
                ready
            );

            // FIXME TODO (T137183027): for record/replay to work properly, we need to ALLOW the
            // "Nondeterminstic algorithm" below, but record & replay those scheduler events.  In
            // the meantime, to get recording partially working, we use the dumb/eager policy where
            // we eagerly block on any ExternalBlocking actions, which essentially is the same as
            // not background them at all.
            if self.recordreplay_modes {
                let first_dtid: DetTid = *self
                    .blocked
                    .external_io_blockers
                    .first()
                    .expect("internal logic error"); // See above blockers_empty check.
                if ready.contains(&first_dtid) {
                    info!(
                        "[step2] Reschedule formerly (external IO) blocked dtid {:?}",
                        first_dtid
                    );
                    self.blocked.external_io_blockers.remove(&first_dtid);
                    self.run_queue.push_eager_io_repoll(first_dtid);
                    return Ok(());
                } else {
                    // FIXME TODO (T137183027): We implement a busy-wait by going around the scheduler loop again.
                    trace!(
                        "[step2] TEMPORARY1: eagerly blocking on external IO for dtid {:?}.  SPINNING!",
                        first_dtid
                    );
                    std::thread::yield_now();
                    return Err(SkipTurn);
                }
            } // End region which should be deleted.

            // Nondeterminsitic algorithm: the unblocked background action jumps back in randomly.
            if !ready.is_empty() {
                // Policy: our heuristic to mitigate nondeterminism (even with nondeterministic external
                // blocking IO) is to only poll it when there is nothing deterministic that is runnable.
                // TODO: we need to take into account internal polling, which may spin forever.
                for ready_dtid in &ready {
                    // TODO: instead record a nondeterministic scheduler event if something is ready.
                    info!(
                        "[step2] NONDET: Reschedule formerly (external IO) blocked dtid {:?}",
                        ready_dtid
                    );
                    self.blocked.external_io_blockers.remove(ready_dtid);
                    self.run_queue.push_eager_io_repoll(*ready_dtid);
                }
                let empty_but_for_pollers = if let Some(fp) = self.run_queue.first_priority() {
                    fp >= LAST_PRIORITY
                } else {
                    true
                };
                if !empty_but_for_pollers {
                    tracing::warn!(
                        "Nondeterministic external actions {:?} jumped in the middle of runnable work ({} tasks). Need to record this for reproducibility.",
                        &ready,
                        self.run_queue.len()
                    );
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
                let (event_ns, dtid) = self
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
                self.wake_timed_event(event_ns, dtid);
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
                &self.run_queue,
                self.blocked.external_io_blockers
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
            let next_dtid = self.run_queue.tentative_pop_next().expect("impossible");
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
            dettid,
            &self.run_queue
        );
        self.skip_turn()
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
                dettid,
                &self.run_queue
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
            dettid,
            req,
            runnable_req
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
                        dettid,
                        target_ns,
                        self.committed_time
                    );
                    Ok(())
                } else {
                    trace!(
                        "[dtid {}] time-based action not ready yet, registering waiter at future time {}. Current time is {}",
                        dettid,
                        target_ns,
                        self.committed_time
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
            ResourceID::BlockingExternalIO => {
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
                self.unblock_guest(dettid, resp);

                // Only once the ivars are cleared, and the guest is officially past the
                // BlockingExternalIO phase ready to issue BlockedExternalContinue, do we
                // then put it into the external_io_blockers struct.
                self.blocked.external_io_blockers.insert(dettid);
                Err(SkipTurn)
            }

            // Thread CONTINUES after completing [potentially] blocking IO.
            ResourceID::BlockedExternalContinue => {
                // We leave the thread out of the run-queue.  At the point we put it back
                // in, this resource request is immediately granted.
                Ok(())
            }

            // Thread requests change in priority
            ResourceID::PriorityChangePoint(prio, change_time) => {
                self.perform_priority_changepoint(dettid, *prio, *change_time)
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
            ResourceID::Exit(_) => Ok(()),
            ResourceID::ParentContinue() => Ok(()),
            ResourceID::InternalIOPolling => Ok(()),
            ResourceID::FutexWait => Ok(()),
            ResourceID::TraceReplay => Ok(()),
        }
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
            dettid,
            new_priority,
            self.priorities
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
    ) -> Result<(), SkipTurn> {
        assert!(runqueue::is_ordinary_priority(new_priority));
        // Alter the threads priority and requeue.
        let old_priority = self.priorities.insert(dettid, new_priority);

        if let Some(pw) = &mut self.preemption_writer {
            let old_prio = old_priority.unwrap();
            debug!(
                "[dtid {}] Recording preemption point, current time {} prior priority {} (next priority {})",
                dettid, guest_time, old_prio, new_priority
            );
            pw.insert_reprioritization(dettid, guest_time, old_prio, new_priority);
            pw.set_current(dettid, new_priority);
        }

        let popped = self.run_queue.commit_tentative_pop(); // Begun in step3.
        assert_eq!(dettid, popped);
        self.runqueue_push_back(dettid); // Repush with new priority
        trace!(
            "[dettid {}] changepoint: Priority mapping after change to priority {}: {:?}",
            dettid,
            new_priority,
            self.priorities
        );

        // Update request to be empty so the thread is unconditionally
        // runnable when it next comes up in the queue.
        let empty_req = Ivar::full(Ok(Resources::new(dettid)));
        trace!(
            "[dettid {}] Priority change point emplaced empty resource request at new {}",
            dettid,
            empty_req
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
                detlog_debug!(
                    "[sched] advance global time for scheduler turn, new time {:?}",
                    newtime,
                );
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
                info!(
                    "[sched-step5] >>>>>>>\n\n COMMIT turn {}, dettid {} using resources {:?}, on previously committed {}",
                    self.turn, next_dtid, rsrcs.resources, self.committed_time
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
            &resp,
            &dtid
        );
        self.clear_nextturn(dtid);
        resp.put(SchedResponse::Go(None));
    }

    /// Clear the thread's nextturn, installing fresh ivars.
    ///
    /// Precondition: guest is stopped so that there is no chance the ivars are being used
    /// concurrently while they are being cleared.
    fn clear_nextturn(&mut self, dtid: DetTid) {
        let mut nextturn = self
            .next_turns
            .get_mut(&dtid)
            .expect("clear_nextturn: Thread should be available in next_turns");
        nextturn.req = Ivar::new();
        nextturn.resp = Ivar::new();
    }

    /// Step: reenqueue the thread that just had a turn.
    fn step6_reenquue(&mut self, next_dtid: DetTid) {
        // We delay popping till here, so while holding the lock we "atomically" move the
        // thread from the front to the back of the queue.
        let dt2 = self.run_queue.commit_tentative_pop();
        assert_eq!(next_dtid, dt2);
        let pos = self.runqueue_push_back(next_dtid);
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
        let replay = self.replay_cursor.is_some();
        let record = self.preemption_writer.is_some();
        if !(replay || record) {
            return;
        }
        let thread_time = global_time.lock().unwrap().threads_time(dettid);
        debug!(
            "simulate exit posthook on tid {}, thread time {}: {:?}",
            dettid, thread_time, placeholder_syscall
        );

        let ev = SchedEvent::syscall(dettid, placeholder_syscall.number(), SyscallPhase::Posthook)
            .with_time(thread_time);
        let print_stack1 = if replay {
            let ConsumeResult {
                keep_running,
                print_stack,
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
            false
        };
        let print_stack2 = record && self.record_event(&ev);
        if print_stack1 || print_stack2 {
            eprintln!(
                ">>> Guest tid {}, at thread time {}, backtrace requested but not available post-exit!\n",
                dettid, thread_time
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

    /// Push_front a thread onto the runqueue, respecting its persistent priority
    /// value. This should be the ordinary way threads are pushed onto the queue.
    fn runqueue_push_front(&mut self, dettid: DetTid) -> PrioritizedOrder {
        let priority = self.get_priority(dettid);
        self.run_queue.push_front(dettid, priority)
    }

    /// Check if a thread is alive, but removed from run queue.
    fn _thread_is_blocked(&self, dtid: DetTid) -> bool {
        if self.run_queue.contains_tid(dtid) {
            false
        } else {
            // Check all the places a blocked flag could be hiding.
            for v in self.blocked.futex_waiters.values() {
                for (dt, _) in v {
                    if *dt == dtid {
                        return false;
                    }
                }
            }
            for (_, dt) in self.blocked.timed_waiters.iter() {
                if dt == dtid {
                    return false;
                }
            }
            true
        }
    }

    /// Summarize the state of the scheduler (verbose).
    pub fn full_summary(&self) -> String {
        let mut buf = String::new();
        write!(&mut buf, "  {}", &self.run_queue).unwrap();

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
    pub fn record_event(&mut self, ev: &SchedEvent) -> bool {
        debug!(
            "[detcore, dtid {}] Record scheduled event #{}: {:?}",
            &ev.dettid, self.recorded_event_count, ev
        );
        let pw = self
            .preemption_writer
            .as_mut()
            .expect("trace_schedevent should be called only when preemption_writer is set");
        pw.insert_schedevent(ev.clone());

        let print_stack = self.try_pop_stacktrace_event(self.recorded_event_count);
        self.recorded_event_count += 1;
        print_stack
    }
}

#[cfg(test)]
mod test {
    use super::*;
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
        tracing_subscriber::fmt::init();
        tree.final_report();
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

        tracing_subscriber::fmt::init();
        tree.final_report();
    }
}
