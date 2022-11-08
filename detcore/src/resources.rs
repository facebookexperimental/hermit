// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

//! Modeling resource types on which guest programs have side effects.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;

use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;

use crate::types::DetInode;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

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
    Exit(bool),

    /// A thread checks in with the scheduler before it continues executing after cloning
    /// a child thread.  This allows it to be prioritized correctly vis-a-vis its child
    /// thread.
    ParentContinue(),

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
    BlockingExternalIO,

    /// Permission to CONTINUE execution after returning from a potentially-blocking
    /// operation that reaches outside the container.
    BlockedExternalContinue,

    /// Permission to continue beyond a priority change point. Used for chaotic
    /// scheduling. Could also be considered less of a resource and more of a
    /// "request" to the scheduler to alter priority of the requesting thread.
    /// The contained value should be a random u64 from the deterministic PRNG.
    /// No guarantees are made about how it will be used.
    ///
    /// Also includes the local time at which the guest observed the preemption point.
    PriorityChangePoint(u64, LogicalTime),
}

/// Permission to a device, which behaves like a predefined "inode".
/// These correspond to special files under /dev
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
#[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
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
}

impl Resources {
    /// Allocate a new, empty Resources request.
    pub fn new(tid: DetTid) -> Resources {
        Resources {
            tid,
            resources: HashMap::new(),
            poll_attempt: 0,
            fyi: String::new(),
        }
    }

    /// Similar to extend, but union all permissions within the intersection.
    /// Union can only be called on resources with matching `tid`.
    /// (NB: equivalent to Haskell `Data.Map.unionWith perm_union`)
    pub fn union(&mut self, other: &Resources) {
        assert_eq!(self.tid, other.tid);
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
        let is_exit_group = self.resources.contains_key(&ResourceID::Exit(true));
        let is_exit = self.resources.contains_key(&ResourceID::Exit(false));
        if (is_exit_group || is_exit) && self.resources.len() > 1 {
            panic!(
                "is_polling_turn: not expecting an InternalIOPolling mixed in with other resource requests: {:?}",
                self
            );
        }
        if is_exit_group {
            Some(Syscall::ExitGroup(Default::default())) // This status code is dummy.
        } else if is_exit {
            Some(Syscall::Exit(Default::default())) // This status code is dummy.
        } else {
            None
        }
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
