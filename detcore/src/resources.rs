/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Modeling resource types on which guest programs have side effects.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::path::PathBuf;

use reverie::Errno;
use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;

use crate::types::ChildWaitSpec;
use crate::types::DetInode;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;
use crate::types::RcbTimeMultiplier;
use crate::types::SigWrapper;

/// Identifies the outer resource turn for a physically nonblocking, guest-internal pipe
/// operation on a backend whose internal pipe turns are host-timed
/// (`BackendCapabilities::internal_pipe_turns_are_host_timed`; today only SaBRe). Such a
/// backend reports these inherited stdio pipes as device resources before Detcore's
/// `InternalIOPolling` turn, so the scheduler tags the outer turn for the same retry-count
/// normalization as the polling turn itself. The string value is a DETLOG marker that the log
/// verifier matches, so it keeps its historical name.
pub(crate) const HOST_TIMED_INTERNAL_PIPE_IO_FYI: &str = "sabre-internal-pipe-io";

/// Identifies the strong one-turn yield issued before a task with a loopback peer performs
/// a zero-timeout poll, on a backend whose loopback pollers yield to their peers
/// (`BackendCapabilities::loopback_pollers_yield_to_peers`; today only SaBRe). The string value
/// is a DETLOG marker that the log verifier matches, so it keeps its historical name. The number of these guest polling-loop iterations depends on when the
/// peer's kernel readiness becomes visible, so the verifier normalizes their scheduler-only turns.
pub(crate) const LOOPBACK_POLL_YIELD_FYI: &str = "sabre-loopback-poll-zero-timeout";

/// Identifies an emulated `pause` request (`SleepUntil(INDEFINITE)`), which a SIGALRM
/// ledger entry ends (`scheduler::sigalrm`), as does a cross-task signal that interrupts it
/// (`scheduler::sleep_signal`). A `nanosleep` whose deadline saturates files the same
/// resource, and is not a pause.
pub(crate) const PAUSE_FYI: &str = "pause";

/// Identifies an emulated `nanosleep` or `clock_nanosleep` request (`SleepUntil(deadline)`),
/// which a cross-task signal that interrupts it ends before its deadline
/// (`scheduler::sleep_signal`).
pub(crate) const NANOSLEEP_FYI: &str = "nanosleep";

/// The SIGALRM action read in the owner turn of an emulated in-guest sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SigalrmSleepDisposition {
    Default,
    Ignored,
    Handled,
}

/// Signal state attached to one exact emulated sleep, never a global cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InGuestSleepSigalrm {
    pub disposition: SigalrmSleepDisposition,
    pub blocked: bool,
}

impl InGuestSleepSigalrm {
    pub(crate) fn ends_process(self) -> bool {
        self.disposition == SigalrmSleepDisposition::Default && !self.blocked
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1151)
/// An exact slowdown-factor transition recorded at a scheduler commit boundary.
#[derive(PartialEq, Debug, Eq, Clone, Copy, Serialize, Deserialize, Hash)]
pub struct ChaosEpochTransition {
    /// Per-thread absolute logical time at which the new epoch begins.
    pub logical_time: LogicalTime,
    /// Deterministic epoch number.
    pub epoch: u64,
    /// Exact fixed-point factor used for virtual-time progression.
    pub factor: RcbTimeMultiplier,
}

/// Identity of one syscall executing outside Hermit's serialized guest turns.
#[derive(
    PartialEq,
    Debug,
    Eq,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    Hash,
    PartialOrd,
    Ord
)]
pub struct ExternalOpId {
    /// Thread that started the operation.
    pub tid: DetTid,
    /// Per-thread syscall sequence number.
    pub sequence: u64,
}

impl ExternalOpId {
    /// Construct an external operation identity.
    pub const fn new(tid: DetTid, sequence: u64) -> Self {
        Self { tid, sequence }
    }
}

/*
NOTE [Blocking Syscalls via Internal Polling]
---------------------------------------------

A system like hermit which sequentializes execution must have the ability to detect
blocking operations, so as not to deadlock the system. (This remains true irrespective of
whether sequentialization is relaxed to admit some parallelism.)

One option is for hermit to *precisely* model everything that may cause a syscall to
block, and run the syscall only when it is certain that it can complete.  This is the
approach with the precise implementation of futexes, for example, but it is tricky in
general and requires hermit recapitulate a lot of the logic in the Linux kernel.

Hence the need for this quick-and-dirty alternative of polling for internal blocking
operations. The idea is that many potentially-blocking syscalls have the option to be run
in a non-blocking fashion. This concept is captured by the `NonBlockableSyscall` trait.

The way the polling strategy works is that the handler inside the guest converts the
syscall to its nonblocking form (or ensures it runs nonblocking via file descriptor
configuration).  The handler executes the syscall in a loop, while increasing the
`poll_attempt` to make it visible to the scheduler that these are retries of a [logically]
blocking operation, rather than new operations.

The scheduler can then interleave these polling operations into the deterministic
linearization of all guest side effects, just like any other operations.  However, it is a
good idea to have some form of "backoff" when polling, because this strategy can
asymptotically increase the total number of syscalls performed. For example, there are a
large number of threads polling, and one thread productively running, we don't want to
poll every thread once for every step of productive work. Conversely, we want to guarantee
that no poller is starved indefinitely, because that could lead to live lock in the
presence of any kind of "busy waiting".

Determinism And linearization assumptions
-----------------------------------------

Why is this strategy deterministic?  How can we guarantee that on two separate executions,
the same syscall `S` will retry exactly `N` times at times `T_1 ... T_n`?

The assumption we make here is that all syscalls, even across multiple cores, act as
atomic operations on the kernel state --- from the guest's perspective.  That is, halfway
through executing a guest, we can describe the prior history of system as the linear
series of syscalls committed to the kernel. Does this actually hold for Linux?  It doesn't
seem to be documented anywhere, and Linux doesn't have anything like a formal semantics.
For most syscalls we treat their man pages as the spec, and they don't answer questions
like this.

If we execute a guest entirely on one core, then only a weaker form of the assumption
needs to hold.

The upshot is, because we make this "linearizability of syscalls" assumption, each polling
attempt for a given syscall is assumed to deterministically succeed or fail based on the
history of syscalls that committed before it.

The troublesome race
--------------------

One may wonder why not simply implement polling by *letting* the operation block,
backgrounding the thread, and then checking in at each polling point to see if the
operation has completed *without* incurring a user/kernel context switch and executing the
syscall again. This would indeed be great, but it would require an oracle that can answer
the question: "has thread X unblocked yet"?

### A non-solution dead-end:

We can recapture control flow immediately after any blocking syscall (in the handler), but
that "posthook" won't run until (1) the syscall unblocks, and (2) the OS gets around to
actually scheduling the thread.  Thus, inside the scheduler, we are racing with threads
like this that are unblocked and in the process of attempting to report for duty.  Such
threads can signal their readiness by tweaking the scheduler data structures, but those
updates arrive completely asynchronously.  For example, if a syscall unblocks on scheduler
turn 100 in one run, how can we know it won't arrive late at turn 101 on the next run?  We
can't determinize the point that the formerly-blocked thread rejoins the scheduler pool.
Not unless we do some huge (realtime) wait at each scheduler turn to ensure the OS has had
a chance to run any recently-unblocked threads!

### Future work: efficient oracle

This nondeterministic-rejoining-the-pool strategy is exactly what we do for *externally*
blocked syscalls.  But for internally-blocked ones that we want to determinize, then we
would need a more efficient "has-unblocked?" oracle.  For example, if we had a kernel
module exposing a safe predicate function that reveals this information to userspace. It's
possible that the /proc file system exposes enough information to synchronously poll a
threads status.

But solving this may be extremely difficult, or impossible.  If the syscall that ran
before us unblocked a thread, how long does that status update take to propogate through
the kernel itself?  If one futex wake syscall unblocks 1000 other threads, their status
would have to atomically change immediately by the time the futex wake returns to the
guest, or at least by the start of the next syscall (or oracle call).

This atomicity property seems hard to guarantee for the OS. Instead, the current polling
strategy that uses nonblocking syscalls is much more localized.  It doesn't treat the
kernel state as monolithic and require atomic updates from the guest perspective, but
rather it only requires atomic treatment of whatever state is relevant to syscalls
operating on that individual resource (futex, file descriptor etc) --- a much less
stringent requirement.

*/

/// Identify a resource which is subject to side effects by actions
/// (system calls) taken by the detcore guest.  Some actions may affect multiple resources.
///
/// Some resources may be entangled or aliased, as when address spaces share pages.
///
/// Resource requests may also be asking simply for permission to proceed, as with exit or
/// sleep.
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize, Hash)]
pub enum ResourceID {
    /// File contents identified by inode.
    FileContents(DetInode),

    /// File and directory Metadata.  These correspond to inodes, because hardlinks to the same file
    /// still share metadata such as modtimes and permissions.
    FileMetadata(DetInode),

    /// Directory contents (entries) identified by inode.
    DirectoryContents(DetInode),

    /// Memory address space identified by process id.  Note that these may be
    /// connected/aliased via shared memory. TODO(T78055411)
    ///
    /// This resource is not typically released/returned, and rather is associated with
    /// the thread on each time slice it runs for the lifetime of the thread.  (However,
    /// it is in principle possible to support unmapping shared mappings that would
    /// dynamically *decrease* the conflicts for scheduling a given thread/process.)
    MemAddrSpace(DetPid),

    /// Permission to change the single resource at this path (file, directory, pipe etc), via
    /// renaming, unlinking etc.  Holding this resource does not grant permission to change the
    /// contents.  Read permission on the path is required just for resolving the path to inode
    /// mapping, but the global method `resolve_path` can be used to abstract this.
    Path(PathBuf),

    /// NB: unstable:
    ///
    /// Transitive permission to all paths underneath a given directory, i.e., all paths that share
    /// this path as a prefix.
    PathsTransitive(PathBuf),

    /// Permission to a device, which behaves like a predefined "inode".
    Device(Device),

    /// Wait for go-ahead to Exit or ExitGroup.  True indicates "group".
    /// Does not need to be released/returned once granted.
    Exit {
        /// Whether this is `exit_group` rather than `exit`.
        group: bool,
        /// Process whose lifecycle state is affected.
        process: DetPid,
        /// Address space used by clear-child-tid futex wakeups.
        mm: MmId,
    },

    /// A thread checks in with the scheduler before it continues executing after cloning
    /// a child thread.  This allows it to be prioritized correctly vis-a-vis its child
    /// thread.
    ParentContinue { parent: DetTid, child: DetTid },

    /// Wait for permission to wake up. This is parameterized by an *absolute* time
    /// (i.e. global time).
    SleepUntil(LogicalTime),

    /// A guest-internal internal IO op which is guaranteed to be nonblocking,
    /// i.e. converted into a polling strategy.
    InternalIOPolling,

    /// An internal event that is only used when implementing context switches under tracereplay.
    /// It should not bump global time.
    TraceReplay,

    /// A guest is blocking on a futex wait, which means it's out of the runqueue.
    FutexWait,

    /// Permission to perform blocking IO with an endpoint outside the deterministic container.
    /// In general these should be recorded if strict reproducibility is to be achieved.
    BlockingExternalIO(ExternalOpId),

    // TODO-HUMAN-REVIEW(PR-868): Review the vfork registration scheduler token.
    /// A `CLONE_VFORK` parent entering the kernel. The scheduler must not admit
    /// another guest turn until the child has registered or the clone has failed.
    BlockingVfork(ExternalOpId),

    // TODO-HUMAN-REVIEW(PR-1152): Review failed deferred-vfork cancellation.
    /// A clone operation governed by [`ResourceID::BlockingVfork`] failed before a child could
    /// register. This is a continuation outcome rather than a new blocking operation: it lets the
    /// scheduler cancel the pending vfork barrier, re-admit the parent, and preserve the original
    /// injected syscall error.
    VforkFailed(ExternalOpId),

    /// Permission to CONTINUE execution after returning from a potentially-blocking
    /// operation that reaches outside the container.
    BlockedExternalContinue(ExternalOpId),

    /// Permission to continue beyond a priority change point. Used for chaotic
    /// scheduling. Could also be considered less of a resource and more of a
    /// "request" to the scheduler to alter priority of the requesting thread.
    /// The contained value should be a random u64 from the deterministic PRNG.
    /// No guarantees are made about how it will be used.
    ///
    /// Also includes the local time at which the guest observed the preemption point.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-1151)
    PriorityChangePoint(u64, LogicalTime, u64, Vec<ChaosEpochTransition>),

    /// Park until an exact or any-child process wait has a logical exit to reap.
    WaitChild {
        /// The process issuing the wait.
        parent: DetPid,
        /// Child population selected by the syscall.
        spec: ChildWaitSpec,
    },

    /// Park until a backend reports that a logically exited child is physically waitable.
    WaitPhysicalChild(DetPid),

    /// A physical signal has been received by the thread, request to continue delivering it and
    /// invoking the signal handler as the next thing to run.
    InboundSignal(SigWrapper),

    /// A scheduler notification that one or more guest-generated signals are
    /// physically pending. An unmarked request wakes waitid polling; a request
    /// carrying `signal_interrupt_errno` wakes internal IO polling. Kept as one
    /// resource because scheduler requests currently admit one resource only.
    WaitidSignals(Vec<SigWrapper>),

    /// Relinquish the current scheduler turn without changing the thread's
    /// persistent priority.
    SchedYield,

    /// A guest thread checking in with the scheduler at a happens-before anchor
    /// point, carrying the thread's running syscall count. The scheduler
    /// consults the configured `HappensBeforeProgram`: it fires any anchors this
    /// checkpoint reaches, and parks the thread (removing it from the run queue)
    /// when the checkpoint is the AFTER endpoint of a Hard edge whose BEFORE
    /// endpoint has not yet fired. The contained value is the post-increment
    /// syscall count observed by the guest.
    HappensBeforeCheckpoint(u64),

    /// A happens-before checkpoint at a syscall that is the `nth` occurrence of
    /// one or more syscall-occurrence anchors on this thread (a named syscall,
    /// optionally on one file descriptor, counted by the guest per thread).
    /// Carries the post-increment syscall count, so count anchors at the same
    /// syscall are evaluated too, and the names of the anchors reached. The
    /// scheduler fires those that resolve to this thread.
    HappensBeforeSyscallCheckpoint {
        /// The post-increment syscall count observed by the guest.
        count: u64,
        /// The syscall-occurrence anchors this syscall is the `nth` occurrence
        /// of, in name order.
        anchors: Vec<String>,
    },

    /// A syscall entry that reached posthook happens-before anchors returned an
    /// internal restart code, so whether it completes is decided by the kernel
    /// after Hermit's checkpoint. The scheduler refuses the run
    /// (`HERMIT_HB_POSTHOOK_INTERRUPTED`) and fires nothing
    /// (<https://github.com/rrnewton/hermit/issues/3929>).
    HappensBeforePosthookInterrupted {
        /// The post-increment syscall count observed by the guest.
        count: u64,
        /// The posthook anchors the entry reached, in name order.
        anchors: Vec<String>,
        /// The restart code, by name.
        code: String,
    },

    /// A real `rt_sigsuspend` executing outside the runnable set while the kernel
    /// atomically installs its temporary signal mask. Unlike arbitrary external
    /// IO, this operation cannot complete without a signal.
    BlockingRtSigsuspend(ExternalOpId),

    /// A one-shot SIGALRM consumed while its single-threaded in-guest target
    /// was parked at a qualified emulated sleep. This authorizes only the
    /// original sleep's typed fatal response, not a generic physical signal.
    InGuestFatalAlarm,
}

/// Permission to a device, which behaves like a predefined "inode".
/// These correspond to special files under /dev
#[allow(clippy::enum_variant_names)]
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize, Hash)]
pub enum Device {
    /// The Stdin, not just from the current process, but from the entire sandboxed job. This
    /// functions similar to a special, predefined inode.
    ContainerStdin,
    /// The Stdout, not just from the current process, but from the entire sandboxed job. This
    /// functions similar to a special, predefined inode.
    ContainerStdout,
    /// The Stderr, not just from the current process, but from the entire sandboxed job. This
    /// functions similar to a special, predefined inode.
    ContainerStderr,
}

// TODO:
// EntanglementEdges: connect together resources that are aliased or connected, including memory
// address spaces.
//

/// `Resources` is a request to lock zero or more resources so that the thread can perform an action.
///
/// There can only be one outstanding request at a time for a given TID.
#[derive(PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct Resources {
    /// The thread ID requesting the resources.
    pub tid: DetTid,
    /// The set of resources requested.
    pub resources: HashMap<ResourceID, Permission>,
    /// If the guest thread is polling the resource, retrying until success, it should
    /// increment this field after each attempt, as a hint to the scheduler to deprioritize it.
    /// That is, requests with a nonzero value will be deprioritized. Zero values will be treated
    /// normally.
    pub poll_attempt: u32,
    /// A bit of metadata (just for debugging), about what the thread is trying to do with the
    /// resources.
    pub fyi: String,
    /// Errno returned when an established signal interruption happens before a
    /// syscall makes progress. Its presence also asks the caller to classify a
    /// signal wake after partial progress, when Linux returns the byte count
    /// instead. `None` does not make an `InternalIOPolling` request eligible
    /// for the scheduler's cross-task signal wakeup.
    #[serde(default)]
    pub(crate) signal_interrupt_errno: Option<i32>,
    /// Set by `tool_global::resource_request` when this request is made while
    /// the thread is handling a syscall whose cost is withheld because it
    /// belongs to a backend-runtime bootstrap window (see
    /// `ThreadState::charge_syscall_time`). The scheduler withholds the
    /// per-turn scheduler time of such a turn, unless it is an IO-polling
    /// retry, for the same reason the syscall cost is withheld: the turn
    /// exists only because the backend's runtime made the syscall, so charging
    /// it would make guest-visible virtual time depend on the backend. `false`
    /// for every request from a backend without such a runtime, and for
    /// requests that do not go through `resource_request`.
    #[serde(default)]
    pub(crate) backend_runtime_bootstrap: bool,
    /// The signal mask a thread sleeps under in the blocking call this request
    /// begins (`BlockingExternalIO`, `BlockingRtSigsuspend`), when the call
    /// installs its own: the mask argument of `rt_sigsuspend`, `ppoll` or
    /// `pselect6`, read in the thread's turn, without `SIGKILL` and `SIGSTOP`,
    /// which Linux never blocks. `None` for a call that sleeps under the
    /// thread's own mask; the scheduler then reads that mask when it commits
    /// the request, while the thread is still stopped at it. The scheduler
    /// records the mask either way, because once the thread runs the call its
    /// live mask is no longer ordered by the schedule, and a `SIGCHLD` target
    /// must be chosen from masks that are
    /// (<https://github.com/rrnewton/hermit/issues/3146>).
    #[serde(default)]
    pub(crate) blocked_signal_mask: Option<u64>,
    /// Set on a `BlockingExternalIO` request whose call installs a temporary
    /// signal mask that the thread's turn cannot vouch for: a pselect6 in
    /// record and replay, whose `{ sigmask, sigsetsize }` wrapper may sit in
    /// memory a tracer read cannot reach though the kernel can, and whose mask
    /// a sibling may rewrite between this turn and the kernel's copy of it.
    /// A masked record/replay ppoll or epoll_pwait sleeps under a private copy
    /// Hermit knows, and sets it too, so that recording and replay, which
    /// serves the call from the log without the kernel installing anything,
    /// schedule the call alike. The
    /// scheduler then records no sleeping mask for the thread
    /// (`BlockedPool::out_of_scheduler_masks` holds `None`), so it neither arms
    /// the thread for a committed signal nor chooses it as a `SIGCHLD` target,
    /// as for every call outside the schedule before the scheduler modelled
    /// signal targets (review of <https://github.com/rrnewton/hermit/pull/3989>).
    #[serde(default)]
    pub(crate) blocked_signal_mask_unknown: bool,
    /// Set on a `BlockingExternalIO` request whose call `hermit replay` serves
    /// from the recording. Such a call runs in the background as it did during
    /// recording, so the schedule matches the recording's, but it cannot
    /// block. The scheduler therefore commits no other turn until it has
    /// finished, and readmits such calls before any other background call, so
    /// that neither its effects nor when it rejoins depend on host timing.
    /// `false` outside replay.
    #[serde(default)]
    pub(crate) replay_served_from_log: bool,
    /// The absolute virtual deadline of the timed wait that a polling request
    /// (`poll_attempt > 0`) retries: `ppoll`, `poll`, `epoll_wait`,
    /// `epoll_pwait`, `rt_sigtimedwait`, `select`, `pselect6` or a polling
    /// `FUTEX_WAIT` with a timeout. Once committed virtual time passes it, the
    /// scheduler restores the backed-off poller's normal priority so its next
    /// retry can return the timeout
    /// (`Scheduler::step2b_restore_due_pollers`,
    /// <https://github.com/rrnewton/hermit/issues/3952>). `None` for every other
    /// request, and always under record/replay.
    #[serde(default)]
    pub(crate) poll_deadline: Option<LogicalTime>,
    /// The action and guest mask read in this exact emulated sleep's own turn.
    /// Only its singleton SleepUntil request may use this snapshot.
    #[serde(default)]
    pub(crate) in_guest_sleep_sigalrm: Option<InGuestSleepSigalrm>,
}

impl fmt::Debug for Resources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("Resources");
        debug
            .field("tid", &self.tid)
            .field("resources", &self.resources)
            .field("poll_attempt", &self.poll_attempt)
            .field("fyi", &self.fyi);
        if let Some(errno) = self.signal_interrupt_errno {
            debug.field("signal_interrupt_errno", &errno);
        }
        if self.backend_runtime_bootstrap {
            debug.field("backend_runtime_bootstrap", &true);
        }
        if let Some(mask) = self.blocked_signal_mask {
            debug.field("blocked_signal_mask", &mask);
        }
        if self.blocked_signal_mask_unknown {
            debug.field("blocked_signal_mask_unknown", &true);
        }
        if self.replay_served_from_log {
            debug.field("replay_served_from_log", &true);
        }
        if let Some(deadline) = self.poll_deadline {
            debug.field("poll_deadline", &deadline);
        }
        if let Some(snapshot) = self.in_guest_sleep_sigalrm {
            debug.field("in_guest_sleep_sigalrm", &snapshot);
        }
        debug.finish()
    }
}

impl Resources {
    /// Allocate a new, empty Resources request.
    pub fn new(tid: DetTid) -> Resources {
        Resources {
            tid,
            resources: HashMap::new(),
            poll_attempt: 0,
            fyi: String::new(),
            signal_interrupt_errno: None,
            backend_runtime_bootstrap: false,
            blocked_signal_mask: None,
            blocked_signal_mask_unknown: false,
            replay_served_from_log: false,
            poll_deadline: None,
            in_guest_sleep_sigalrm: None,
        }
    }

    /// Similar to extend, but union all permissions within the intersection.
    /// Union can only be called on resources with matching `tid`.
    /// (NB: equivalent to Haskell `Data.Map.unionWith perm_union`)
    pub fn union(&mut self, other: &Resources) {
        assert_eq!(self.tid, other.tid);
        // Union describes a combined operation, not the exact wait whose owner
        // captured the snapshot. Never promote either operand's authority.
        self.in_guest_sleep_sigalrm = None;
        for (id, perm2) in other.resources.iter() {
            match self.resources.entry(id.clone()) {
                Entry::Occupied(mut e) => {
                    let perm1 = e.get_mut();
                    *perm1 = perm1.union(perm2)
                }
                Entry::Vacant(e) => {
                    e.insert(perm2.clone());
                }
            }
        }
        match (self.signal_interrupt_errno, other.signal_interrupt_errno) {
            (None, interrupt) => self.signal_interrupt_errno = interrupt,
            (Some(left), Some(right)) => assert_eq!(left, right),
            (Some(_), None) => {}
        }
        self.backend_runtime_bootstrap |= other.backend_runtime_bootstrap;
        match (self.blocked_signal_mask, other.blocked_signal_mask) {
            (None, mask) => self.blocked_signal_mask = mask,
            (Some(left), Some(right)) => assert_eq!(left, right),
            (Some(_), None) => {}
        }
        self.blocked_signal_mask_unknown |= other.blocked_signal_mask_unknown;
        self.replay_served_from_log |= other.replay_served_from_log;
        // A merged request waits for the earlier of two deadlines.
        self.poll_deadline = match (self.poll_deadline, other.poll_deadline) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
    }

    /// Return the owner snapshot only for an unmodified singleton sleep.
    pub(crate) fn in_guest_sigalrm_sleep(&self) -> Option<(InGuestSleepSigalrm, LogicalTime)> {
        if self.resources.len() != 1
            || self.poll_attempt != 0
            || self.signal_interrupt_errno.is_some()
            || self.blocked_signal_mask.is_some()
            || self.replay_served_from_log
            || self.backend_runtime_bootstrap
            || self.poll_deadline.is_some()
        {
            return None;
        }
        let (ResourceID::SleepUntil(deadline), Permission::W) = self.resources.iter().next()?
        else {
            return None;
        };
        if !(self.fyi == PAUSE_FYI && deadline.is_indefinite() || self.fyi == NANOSLEEP_FYI) {
            return None;
        }
        Some((self.in_guest_sleep_sigalrm?, *deadline))
    }

    pub fn set_signal_interrupt_errno(&mut self, errno: Errno) {
        self.signal_interrupt_errno = Some(errno.into_raw());
    }

    pub fn signal_interrupt_errno(&self) -> Option<i32> {
        self.signal_interrupt_errno
    }

    /// Insert a new individual resource into a set of resources.
    /// Panics if that resource is already present.
    pub fn insert(&mut self, new: ResourceID, perm: Permission) {
        let old = self.resources.insert(new, perm);
        assert_eq!(old, None);
    }

    /// Add some metadata, typically a short tag, to the FYI.
    pub fn fyi(&mut self, s: &str) {
        if !self.fyi.is_empty() {
            self.fyi.push_str(", ");
        }
        self.fyi.push_str(s);
    }

    // Test if the request corresponds to an exit or exit_group.
    // If it does, return a synthetic copy of the syscall which would generate this resource request.
    pub fn as_exit_syscall(&self) -> Option<Syscall> {
        let exit = self.resources.keys().find_map(|resource| match resource {
            ResourceID::Exit { group, process, mm } => Some((*group, *process, *mm)),
            _ => None,
        });
        if exit.is_some() && self.resources.len() > 1 {
            panic!(
                "is_polling_turn: not expecting an InternalIOPolling mixed in with other resource requests: {:?}",
                self
            );
        }
        if exit.is_some_and(|(group, _, _)| group) {
            Some(Syscall::ExitGroup(Default::default())) // This status code is dummy.
        } else if exit.is_some() {
            Some(Syscall::Exit(Default::default())) // This status code is dummy.
        } else {
            None
        }
    }

    /// Return lifecycle identity when this is an exit request.
    pub fn exit_identity(&self) -> Option<(bool, DetPid, MmId)> {
        self.resources.keys().find_map(|resource| match resource {
            ResourceID::Exit { group, process, mm } => Some((*group, *process, *mm)),
            _ => None,
        })
    }
}

/// Is the resource to be used for writing, reading or both?
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize, Hash)]
pub enum Permission {
    /// Read permission.
    R,
    /// Write permission.
    W,
    /// Read+Write permission.
    RW,
}

impl Permission {
    /// Take the union of requested permissions.
    pub fn union(&self, other: &Permission) -> Permission {
        use Permission::*;
        match (self, other) {
            (R, R) => R,
            (R, W) => RW,
            (W, R) => RW,
            (W, W) => W,
            (RW, _) => RW,
            (_, RW) => RW,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_tokens_do_not_alias_unrelated_operations() {
        let tid1 = DetTid::from_raw(1);
        let tid2 = DetTid::from_raw(2);
        assert_ne!(
            ResourceID::BlockingExternalIO(ExternalOpId::new(tid1, 7)),
            ResourceID::BlockingExternalIO(ExternalOpId::new(tid2, 7))
        );
        assert_ne!(
            ResourceID::BlockingExternalIO(ExternalOpId::new(tid1, 7)),
            ResourceID::BlockingRtSigsuspend(ExternalOpId::new(tid1, 7))
        );
        assert_ne!(
            ResourceID::ParentContinue {
                parent: tid1,
                child: tid2,
            },
            ResourceID::ParentContinue {
                parent: tid2,
                child: tid1,
            }
        );
    }

    fn owner_sleep(deadline: LogicalTime, fyi: &str) -> Resources {
        let mut request = Resources::new(DetTid::from_raw(1));
        request.insert(ResourceID::SleepUntil(deadline), Permission::W);
        request.fyi(fyi);
        request.in_guest_sleep_sigalrm = Some(InGuestSleepSigalrm {
            disposition: SigalrmSleepDisposition::Default,
            blocked: false,
        });
        request
    }

    #[test]
    fn in_guest_sleep_snapshot_requires_exact_singleton_wait_shape() {
        let deadline = LogicalTime::from_nanos(10);
        for (deadline, fyi) in [
            (LogicalTime::INDEFINITE, PAUSE_FYI),
            (deadline, NANOSLEEP_FYI),
        ] {
            let request = owner_sleep(deadline, fyi);
            assert_eq!(
                request.in_guest_sigalrm_sleep(),
                Some((request.in_guest_sleep_sigalrm.unwrap(), deadline))
            );
        }
        for case in 0..11 {
            let mut request = owner_sleep(deadline, NANOSLEEP_FYI);
            match case {
                0 => request.in_guest_sleep_sigalrm = None,
                1 => request.fyi = PAUSE_FYI.to_owned(),
                2 => request.fyi("unrelated"),
                3 => request.insert(ResourceID::TraceReplay, Permission::W),
                4 => {
                    request
                        .resources
                        .insert(ResourceID::SleepUntil(deadline), Permission::R);
                }
                5 => request.poll_attempt = 1,
                6 => request.signal_interrupt_errno = Some(libc::EINTR),
                7 => request.blocked_signal_mask = Some(0),
                8 => request.backend_runtime_bootstrap = true,
                9 => request.poll_deadline = Some(deadline),
                10 => request.replay_served_from_log = true,
                _ => unreachable!(),
            }
            assert_eq!(request.in_guest_sigalrm_sleep(), None, "case {case}");
        }
    }

    #[test]
    fn merging_resources_cannot_spread_sleep_snapshot_authority() {
        let deadline = LogicalTime::from_nanos(10);
        for merge_sleep_first in [false, true] {
            let mut sleep = owner_sleep(deadline, NANOSLEEP_FYI);
            let mut unrelated = Resources::new(sleep.tid);
            unrelated.insert(ResourceID::TraceReplay, Permission::W);
            let merged = if merge_sleep_first {
                sleep.union(&unrelated);
                sleep
            } else {
                unrelated.union(&sleep);
                unrelated
            };
            assert_eq!(merged.in_guest_sleep_sigalrm, None);
            assert_eq!(merged.in_guest_sigalrm_sleep(), None);
        }
        let mut sleep = owner_sleep(deadline, NANOSLEEP_FYI);
        sleep.union(&sleep.clone());
        assert_eq!(
            sleep.in_guest_sleep_sigalrm, None,
            "even identical merged operations surrender their original wait identity"
        );
    }
}
