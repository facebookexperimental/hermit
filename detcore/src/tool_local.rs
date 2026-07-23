/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The process-local portion of the Detcore Reverie-tool.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use detcore_model::pedigree::Pedigree;
use nix::fcntl::AtFlags;
use nix::fcntl::OFlag;
use nix::sys::stat;
use nix::unistd::Pid;
use rand::Rng as _;
use rand::SeedableRng;
use rand_distr::Distribution;
use rand_distr::Exp;
use rand_pcg::Pcg64Mcg;
use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::CloneFlags;
use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;

use crate::config::Config;
use crate::detlog;
use crate::fd::*;
use crate::memory::MemoryMetadata;
use crate::preemptions::ThreadHistoryIterator;
use crate::record_or_replay::NoopTool;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::scheduler::Priority;
use crate::stat::*;
use crate::types::*;
use crate::util::rcbs_to_duration;

/// The detcore tool and its per-process state.
#[derive(Debug, Serialize, Deserialize)]
pub struct Detcore<T = NoopTool> {
    //
    // TODO:
    //  - Add Pid cache here.
    //
    /// The detpid of this process.
    pub(crate) detpid: DetPid,

    /// Cached copy of the tool Config.  Immutable over the lifetime of the program.
    pub(crate) cfg: Config,

    /// The record or replay sub-tool. Any events that cannot be made
    /// deterministic are forwarded to this tool. Thus, Detcore acts as a
    /// filter-map for syscalls.
    pub(crate) record_or_replay: T,
}

/// The metadata associated with the file system view of a particular *process*.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Identity of the Linux descriptor table represented by `file_handles`.
    pub(crate) files_id: FilesId,
    /// Sequence used to allocate open file descriptions observed through this table.
    next_open_file_sequence: u64,
    /// Track what file handles actually point to (e.g. after dup2).
    /// This includes both the identifying resource (usually inode) and the deterministic file handle.
    pub(crate) file_handles: HashMap<RawFd, DetFd>,
}

/// A single POSIX per-process interval timer created by `timer_create(2)`.
///
/// Detcore tracks enough state to make the `timer_*` syscalls deterministic
/// under `--strict`, but it does **not** deliver timer-expiration signals: an
/// armed timer is recorded and its remaining time reported against the
/// deterministic virtual clock, yet it never actually fires. This is sufficient
/// for programs that merely arm a long watchdog timer at startup (e.g. CPython
/// arms a 300s `CLOCK_MONOTONIC`/`SIGRTMIN` watchdog and lets the process exit
/// long before it could expire), but a program that depends on receiving the
/// timer signal will not observe it. Deterministic timer-signal delivery is
/// future work.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PosixTimer {
    /// Reload interval for periodic timers, in nanoseconds (0 => one-shot).
    interval_ns: u64,
    /// Absolute virtual-time deadline of the next expiration, or `None` when the
    /// timer is disarmed (`it_value == 0`).
    deadline: Option<LogicalTime>,
}

/// The set of POSIX timers owned by a *process*.
///
/// Timers are shared among all threads of a process and, per POSIX, are **not**
/// inherited across `fork(2)`. Detcore therefore shares this table on
/// `CLONE_THREAD` and starts a fresh, empty table for every new process (see
/// `init_thread_state`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PosixTimers {
    /// Deterministic id allocator. Kernel `timer_t`s are opaque, so we hand out
    /// ids as 0, 1, 2, ... in creation order to keep them reproducible.
    next_id: i32,
    timers: HashMap<i32, PosixTimer>,
}

impl PosixTimers {
    /// Allocate a new (disarmed) timer, returning its deterministic id.
    pub(crate) fn create(&mut self) -> i32 {
        let id = self.next_id;
        self.next_id += 1;
        self.timers.insert(
            id,
            PosixTimer {
                interval_ns: 0,
                deadline: None,
            },
        );
        id
    }

    /// Arm or disarm timer `id`. `interval_ns` is the periodic reload and
    /// `deadline` the absolute virtual-time expiration (the caller derives it
    /// from the request flags and the current virtual clock; `None` disarms).
    /// Returns the previous `(remaining_ns, interval_ns)` for `old_value`, or
    /// `None` if the id is unknown.
    pub(crate) fn settime(
        &mut self,
        id: i32,
        interval_ns: u64,
        deadline: Option<LogicalTime>,
        now: LogicalTime,
    ) -> Option<(u64, u64)> {
        let timer = self.timers.get_mut(&id)?;
        let old = (remaining_ns(timer.deadline, now), timer.interval_ns);
        timer.interval_ns = interval_ns;
        timer.deadline = deadline;
        Some(old)
    }

    /// Report the current `(remaining_ns, interval_ns)` for `timer_gettime`, or
    /// `None` if the id is unknown.
    pub(crate) fn gettime(&self, id: i32, now: LogicalTime) -> Option<(u64, u64)> {
        let timer = self.timers.get(&id)?;
        Some((remaining_ns(timer.deadline, now), timer.interval_ns))
    }

    /// Whether a timer with this id currently exists.
    pub(crate) fn contains(&self, id: i32) -> bool {
        self.timers.contains_key(&id)
    }

    /// Remove a timer; returns whether it existed.
    pub(crate) fn remove(&mut self, id: i32) -> bool {
        self.timers.remove(&id).is_some()
    }
}

/// Nanoseconds remaining until `deadline` relative to `now`, saturating at 0.
/// A disarmed timer (`None`) or an already-elapsed deadline reports 0, which is
/// how the kernel reports an expired/disarmed timer via `timer_gettime`.
fn remaining_ns(deadline: Option<LogicalTime>, now: LogicalTime) -> u64 {
    match deadline {
        Some(d) => d.as_nanos().saturating_sub(now.as_nanos()),
        None => 0,
    }
}

impl<T> Default for Detcore<T> {
    fn default() -> Self {
        // TODO(T77816673): eventually we want to remove this requirement.
        // In the meantime... just don't call this.
        // Instead see the new() method defined in lib.rs
        panic!("Detcore Default impl should not be called");
    }
}

impl<T: RecordOrReplay> AsRef<T> for Detcore<T> {
    fn as_ref(&self) -> &T {
        &self.record_or_replay
    }
}

impl<T: RecordOrReplay> AsMut<T> for Detcore<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.record_or_replay
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Helper function for delegating the injection of a syscall to the
    /// record_or_replay tool.
    ///
    /// It is important to classify the cases where we need to call `inject`. We
    /// have three main choices to make when handling a syscall:
    ///  1. Fully determinize the syscall. In this case, it doesn't need to call
    ///     `inject` at all.
    ///  2. Partially determinize the syscall. In this case, it can't fully
    ///     determinize a syscall but only part of it. For example, a `stat` syscall
    ///     is ultimately non-deterministic because the file may not always exist.
    ///     However, the mtime or inode numbers can be made deterministic.
    ///  3. The syscall cannot be determinized at all. For example, a call to
    ///     `recvfrom` cannot be made deterministic.
    ///
    /// Thus, this is called whenever `inject` would be called for non-bookkeeping
    /// operations.
    pub(crate) async fn record_or_replay<G, S>(
        &self,
        guest: &mut G,
        syscall: S,
    ) -> Result<i64, Errno>
    where
        G: Guest<Self>,
        S: Into<Syscall>,
    {
        self.record_or_replay
            .handle_syscall_event(&mut guest.into_guest(), syscall.into())
            .await
            // TODO: Get rid of this and make this whole function use the Error type.
            .map_err(|err| err.into_errno().unwrap())
    }
}

impl FileMetadata {
    /// create an empty file metadata
    fn new(owner: DetTid) -> Self {
        FileMetadata {
            files_id: FilesId::initial(owner),
            next_open_file_sequence: 0,
            file_handles: HashMap::new(),
        }
    }

    fn allocate_open_file_id(&mut self, creator: DetTid) -> OpenFileId {
        let id = OpenFileId::new(creator, self.next_open_file_sequence);
        self.next_open_file_sequence += 1;
        id
    }

    pub(crate) fn fork_for(&self, child: DetTid) -> Self {
        Self {
            files_id: FilesId::forked(child),
            next_open_file_sequence: self.next_open_file_sequence,
            file_handles: self.file_handles.clone(),
        }
    }

    pub(crate) fn for_exec(&self, task: DetTid) -> Self {
        Self {
            files_id: self.files_id.for_exec(task),
            next_open_file_sequence: self.next_open_file_sequence,
            file_handles: self
                .file_handles
                .iter()
                .filter_map(|(&fd, detfd)| (!detfd.is_cloexec()).then_some((fd, detfd.clone())))
                .collect(),
        }
    }

    pub(crate) fn open_files_closed_on_exec(&self, table_is_shared: bool) -> Vec<OpenFileId> {
        if table_is_shared {
            return Vec::new();
        }

        let mut open_files = HashMap::new();
        for detfd in self.file_handles.values() {
            let id = detfd.open_file_id();
            let total_aliases = detfd.open_file_alias_count();
            let entry = open_files.entry(id).or_insert((0, total_aliases, true));
            debug_assert_eq!(entry.1, total_aliases);
            entry.0 += 1;
            entry.2 &= detfd.is_cloexec();
        }

        let mut closed: Vec<_> = open_files
            .into_iter()
            .filter_map(|(id, (table_aliases, total_aliases, all_cloexec))| {
                (all_cloexec && table_aliases == total_aliases).then_some(id)
            })
            .collect();
        closed.sort();
        closed
    }

    /// set default fds
    fn setup_stdio(mut self, pid: Pid, owner: DetTid) -> Self {
        // guest stdio can be a pipe, which make things difficult
        // hence use a dummy stat here.
        // SAFETY: stating stdin is likely to always be safe
        let stat: DetStat = stat::fstat(unsafe { BorrowedFd::borrow_raw(0) })
            .unwrap()
            .into();
        let stdin = DetFd::new(
            0,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Path(format!("/proc/{}/fd/0", pid).into()));
        let stdout = DetFd::new(
            1,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Path(format!("/proc/{}/fd/1", pid).into()));
        let stderr = DetFd::new(
            2,
            OFlag::empty(),
            FdType::Regular,
            self.allocate_open_file_id(owner),
        )
        .with_stat(stat)
        .with_resource(ResourceID::Path(format!("/proc/{}/fd/2", pid).into()));

        self.add_detfd(stdin);
        self.add_detfd(stdout);
        self.add_detfd(stderr);

        self
    }

    /// get detfd from rawfd, rawfd must be added or dup-ed first.
    fn with_detfd<F, U>(&mut self, fd: RawFd, mut f: F) -> Result<U, Errno>
    where
        F: FnMut(&mut DetFd) -> U,
    {
        let detfd = self.file_handles.get_mut(&fd).ok_or(Errno::EBADF)?;
        Ok(f(detfd))
    }

    /// add a detfd
    fn add_detfd(&mut self, detfd: DetFd) {
        let fd = detfd.fd;
        self.file_handles.insert(fd, detfd);
    }

    /// add a raw fd
    fn add_fd(
        &mut self,
        creator: DetTid,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
        stat: Option<DetStat>,
    ) -> Result<(), Errno> {
        let id = self.allocate_open_file_id(creator);
        let detfd = DetFd::new(fd, flags, ty, id).with_stat(stat);
        self.add_detfd(detfd);
        Ok(())
    }

    /// remove a rawfd
    fn remove_fd(&mut self, fd: RawFd) -> Option<OpenFileId> {
        let detfd = self.file_handles.remove(&fd)?;
        (detfd.open_file_alias_count() == 1).then(|| detfd.open_file_id())
    }

    /// dup raw fds.
    fn dup_fd(
        &mut self,
        oldfd: RawFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, Errno> {
        if oldfd == newfd {
            self.with_detfd(oldfd, |_| ())?;
            return Ok(None);
        }

        let detfd = self.with_detfd(oldfd, |old_detfd| {
            old_detfd.clone().with_fd(newfd).with_fd_flags(flags)
        })?;
        let replaced = self.file_handles.insert(newfd, detfd);
        Ok(replaced
            .and_then(|detfd| (detfd.open_file_alias_count() == 1).then(|| detfd.open_file_id())))
    }
}

#[cfg(test)]
mod posix_timers_tests {
    use super::*;

    fn t(ns: u64) -> LogicalTime {
        LogicalTime::from_nanos(ns)
    }

    #[test]
    fn ids_are_deterministic_and_sequential() {
        let mut timers = PosixTimers::default();
        assert_eq!(timers.create(), 0);
        assert_eq!(timers.create(), 1);
        assert_eq!(timers.create(), 2);
    }

    #[test]
    fn settime_reports_previous_arming_and_remaining_uses_virtual_clock() {
        let mut timers = PosixTimers::default();
        let id = timers.create();

        // Arm a one-shot timer for 100ns at t=0. A freshly created timer was
        // disarmed, so the reported old value is zero.
        let old = timers.settime(id, 0, Some(t(100)), t(0)).expect("known id");
        assert_eq!(old, (0, 0));

        // At t=40 there should be 60ns remaining and no interval.
        assert_eq!(timers.gettime(id, t(40)), Some((60, 0)));
        // Past the deadline the remaining time saturates at 0.
        assert_eq!(timers.gettime(id, t(150)), Some((0, 0)));
    }

    #[test]
    fn resetting_reports_old_remaining() {
        let mut timers = PosixTimers::default();
        let id = timers.create();
        timers.settime(id, 0, Some(t(100)), t(0));
        // Re-arm at t=30 (70ns remained) with a periodic 50ns timer.
        let old = timers
            .settime(id, 50, Some(t(200)), t(30))
            .expect("known id");
        assert_eq!(old, (70, 0));
        assert_eq!(timers.gettime(id, t(30)), Some((170, 50)));
    }

    #[test]
    fn disarm_and_unknown_ids() {
        let mut timers = PosixTimers::default();
        let id = timers.create();
        timers.settime(id, 0, Some(t(100)), t(0));
        // Disarm: value of 0 -> deadline None -> remaining 0.
        timers.settime(id, 0, None, t(10));
        assert_eq!(timers.gettime(id, t(10)), Some((0, 0)));

        // Unknown ids are rejected.
        assert_eq!(timers.settime(99, 0, Some(t(1)), t(0)), None);
        assert_eq!(timers.gettime(99, t(0)), None);
        assert!(!timers.contains(99));
    }

    #[test]
    fn delete_removes_timer() {
        let mut timers = PosixTimers::default();
        let id = timers.create();
        assert!(timers.contains(id));
        assert!(timers.remove(id));
        assert!(!timers.contains(id));
        // Deleting again fails.
        assert!(!timers.remove(id));
    }
}

#[cfg(test)]
mod file_metadata_tests {
    use super::*;

    #[test]
    fn fork_copies_slots_but_preserves_open_file_aliases() {
        let parent_tid = DetTid::from_raw(10);
        let child_tid = DetTid::from_raw(11);
        let mut parent = FileMetadata::new(parent_tid);
        parent
            .add_fd(parent_tid, 3, OFlag::O_NONBLOCK, FdType::Socket, None)
            .expect("parent fd should be inserted");
        parent
            .dup_fd(3, 4, OFlag::O_CLOEXEC)
            .expect("dup should succeed");

        let parent_open = parent
            .with_detfd(3, |fd| fd.open_file_id())
            .expect("parent fd should exist");
        let duplicate_open = parent
            .with_detfd(4, |fd| fd.open_file_id())
            .expect("duplicate fd should exist");
        assert_eq!(parent_open, duplicate_open);

        let mut child = parent.fork_for(child_tid);
        assert_ne!(parent.files_id, child.files_id);
        assert_ne!(
            FdSlot {
                files: parent.files_id,
                fd: 3,
            },
            FdSlot {
                files: child.files_id,
                fd: 3,
            }
        );
        assert_eq!(
            parent_open,
            child
                .with_detfd(3, |fd| fd.open_file_id())
                .expect("forked fd should retain its open file identity")
        );

        parent
            .add_fd(parent_tid, 5, OFlag::empty(), FdType::Regular, None)
            .expect("new parent fd should be inserted");
        child
            .add_fd(child_tid, 5, OFlag::empty(), FdType::Regular, None)
            .expect("new child fd should be inserted");
        assert_ne!(
            parent
                .with_detfd(5, |fd| fd.open_file_id())
                .expect("new parent fd should exist"),
            child
                .with_detfd(5, |fd| fd.open_file_id())
                .expect("new child fd should exist"),
            "separate opens after fork must not alias"
        );
    }

    #[test]
    fn equal_fd_dup_preserves_descriptor_flags() {
        let owner = DetTid::from_raw(20);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::O_CLOEXEC, FdType::Regular, None)
            .expect("fd should be inserted");

        assert_eq!(
            metadata
                .dup_fd(3, 3, OFlag::empty())
                .expect("equal-fd dup should validate the source"),
            None
        );
        assert!(
            metadata
                .with_detfd(3, |fd| fd.is_cloexec())
                .expect("fd should remain present"),
            "dup2(fd, fd) must not clear close-on-exec"
        );
    }

    #[test]
    fn last_open_file_alias_survives_dup_and_fork() {
        let parent_tid = DetTid::from_raw(30);
        let child_tid = DetTid::from_raw(31);
        let mut parent = FileMetadata::new(parent_tid);
        parent
            .add_fd(parent_tid, 3, OFlag::empty(), FdType::Socket, None)
            .expect("socket should be inserted");
        let open_file_id = parent
            .with_detfd(3, |fd| fd.open_file_id())
            .expect("socket should exist");
        assert_eq!(
            parent
                .dup_fd(3, 4, OFlag::empty())
                .expect("dup should succeed"),
            None
        );
        assert_eq!(parent.remove_fd(3), None, "duplicate retains the OFD");

        let mut child = parent.fork_for(child_tid);
        assert_eq!(parent.remove_fd(4), None, "forked child retains the OFD");
        assert_eq!(
            child.remove_fd(4),
            Some(open_file_id),
            "only the final alias releases the OFD"
        );

        let mut replacement = FileMetadata::new(parent_tid);
        replacement
            .add_fd(parent_tid, 3, OFlag::empty(), FdType::Socket, None)
            .expect("source should be inserted");
        replacement
            .add_fd(parent_tid, 4, OFlag::empty(), FdType::Socket, None)
            .expect("target should be inserted");
        let target_id = replacement
            .with_detfd(4, |fd| fd.open_file_id())
            .expect("target should exist");
        assert_eq!(
            replacement
                .dup_fd(3, 4, OFlag::empty())
                .expect("dup replacement should succeed"),
            Some(target_id),
            "replacing the target must release its last OFD alias"
        );
    }

    #[test]
    fn exec_reports_only_cloexec_open_files_with_no_other_aliases() {
        let owner = DetTid::from_raw(40);
        let child_tid = DetTid::from_raw(41);
        let mut metadata = FileMetadata::new(owner);
        metadata
            .add_fd(owner, 3, OFlag::O_CLOEXEC, FdType::Socket, None)
            .expect("socket should be inserted");
        let open_file_id = metadata
            .with_detfd(3, |fd| fd.open_file_id())
            .expect("socket should exist");

        assert_eq!(metadata.open_files_closed_on_exec(false), [open_file_id]);
        assert!(
            metadata.open_files_closed_on_exec(true).is_empty(),
            "a shared descriptor table retains the original slot"
        );

        let child = metadata.fork_for(child_tid);
        assert!(
            metadata.open_files_closed_on_exec(false).is_empty(),
            "a copied table retains an OFD alias"
        );
        drop(child);

        metadata
            .dup_fd(3, 4, OFlag::empty())
            .expect("non-CLOEXEC alias should be created");
        assert!(
            metadata.open_files_closed_on_exec(false).is_empty(),
            "a non-CLOEXEC alias keeps the OFD live across exec"
        );
    }
}

/// Various measurements of one guest thread's execution. This is useful for printing
/// context in logs as we go and printing a final summary.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ThreadStats {
    /// A simple count of how many syscalls have been handled on this thread.
    pub syscall_count: u64,

    /// A count of how many signals have arrived at this thread, total.
    pub signal_count: u64,

    /// How many syscalls this time slice (since last preemption)?
    pub timeslice_syscall_count: u64,

    /// How many signals this time slice (since last preemption)?
    pub timeslice_signal_count: u64,

    /// How many logical timeslices have we completed before the current one?
    /// These correspond to when we are preempted at the `end_of_timeslice`.
    pub timeslice_count: u64,

    /// The timeslice_count for the timeslice which was the last one that had a recorded end time in
    /// the `--replay-preemptions-from` log.
    pub last_recorded_slice: Option<u64>,
}

impl ThreadStats {
    /// Create a new thread stats with zero counters.
    pub fn new() -> Self {
        Default::default()
    }

    // TODO: this can evolve to keep a full histogram:
    /// Increment the count of system calls
    pub fn count_syscall(&mut self) {
        self.syscall_count += 1;
        self.timeslice_syscall_count += 1;
    }

    /// Increment the count of signals.
    pub fn count_signal(&mut self) {
        self.signal_count += 1;
        self.timeslice_signal_count += 1;
    }

    /// Reset counters for a new timeslice.
    /// Increases the count of completed timeslices.
    fn reset_timeslice(&mut self) {
        self.timeslice_syscall_count = 0;
        self.timeslice_signal_count = 0;
        self.timeslice_count += 1;
    }
}

/// Information inherited by a `CLONE_VFORK` child so it can register itself
/// while its parent is blocked inside the kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingVfork {
    pub parent_dettid: DetTid,
    pub parent_detpid: DetPid,
    pub child_tid_addr: usize,
    pub flags: CloneFlags,
    pub child_priority_entropy: Option<u64>,
}

/// The Detcore per-thread state.
#[derive(Serialize, Deserialize, Clone)]
pub struct ThreadState<T> {
    /// The deterministic thread ID of the this thread.
    pub dettid: DetTid,
    /// The deterministic process ID of the this thread.
    pub detpid: Option<DetTid>,

    /// Linux memory address space shared by tasks created with `CLONE_VM`.
    pub mm_id: MmId,

    /// Shared memory mappings used to resolve process-shared futex keys.
    pub(crate) memory_metadata: Arc<Mutex<MemoryMetadata>>,

    /// This threads path within the thread/process ancestry tree. (The terminology comes from
    /// Cilk.)
    pub pedigree: Pedigree,

    /// Counting various events.
    pub stats: ThreadStats,

    /// In chaos mode with --replay-preemptions-from, we hold a list of our future preemption points.
    pub preemption_points: Option<ThreadHistoryIterator>,

    /// User defined interruption points
    pub interrupt_at: BTreeSet<u64>,

    /// clone flags when SYS_clone is called.
    ///
    /// This is just a place to stash the value temporarily, where it can be read out by
    /// the child thread upon `init_thread_state`.  After that point, it is consumed by
    /// the child and becomes `None` again.
    ///
    /// Stated differently, this is just for message-passing communication.
    pub clone_flags: Option<CloneFlags>,

    /// Registration metadata for a vfork child. The child consumes this in
    /// `handle_thread_start`; the parent clears its copy when vfork returns.
    pub pending_vfork: Option<PendingVfork>,

    /// Shared file metadata among all threads in the same process.
    /// Initialized for new threads (shared or fresh), and then overwritten again on `execve`.
    pub file_metadata: Arc<Mutex<FileMetadata>>,

    /// POSIX per-process timers created via `timer_create(2)`. Shared among the
    /// threads of a process (`CLONE_THREAD`) and not inherited across `fork`.
    pub(crate) posix_timers: Arc<Mutex<PosixTimers>>,

    /// pseudo random number state
    pub prng: Pcg64Mcg,

    /// RNG to drive chaos scheduling decisions, separate from other (guest) RNG.
    pub chaos_prng: Pcg64Mcg,

    /// logical time, measuring progress of this thread and only this thread.
    pub thread_logical_time: DetTime,

    /// the last RCB clock value committed to `thread_logical_time`
    pub committed_clock_value: u64,

    /// Thread state associated with record/replay.
    pub record_or_replay: T,

    /// How much longer does this thread get to run before it must check-in with the
    /// scheduler?  Note that this notion of time slice can extend across a region of time
    /// that includes syscalls (and thus handlers).
    ///
    /// If set to `None`, the thread can run indefinitely without preemption.
    ///
    /// This is in units of virtual Nanoseconds.  And it is an exact time in the future,
    /// not a relative duration.
    pub end_of_timeslice: Option<LogicalTime>,

    /// Track what our last timer was set for, just to double check that RCB timers are behaving
    /// as expected and see if we went over.  (For exmaple, this behaves badly if threads are not
    /// pinned and our we migrate between cores.)
    pub last_rcb_timer: Option<u64>,

    /// Are we past the global moment when the guest's first execve of its root binary completes
    /// (with a successful exit code).
    pub(crate) past_global_first_execve: bool,
}

/// We cannot assume that the record_or_replay "subtool" is Debug, so it is handy to be able to
/// print the Detcore threadstate alone.
impl<T> std::fmt::Debug for ThreadState<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadState")
            .field("dettid", &self.dettid)
            .field("detpid", &self.detpid)
            .field("mm_id", &self.mm_id)
            .field("memory_metadata", &self.memory_metadata)
            .field("stats", &self.stats)
            .field("clone_flags", &self.clone_flags)
            .field("file_metadata", &self.file_metadata)
            .field("posix_timers", &self.posix_timers)
            .field("prng", &self.prng)
            .field("chaos_prng", &self.chaos_prng)
            .field("thread_logical_time", &self.thread_logical_time)
            .field("committed_clock_value", &self.committed_clock_value)
            .field("end_of_timeslice", &self.end_of_timeslice)
            .field("last_rcb_timer", &self.last_rcb_timer)
            .finish()
    }
}

impl<T> Default for ThreadState<T> {
    fn default() -> Self {
        unreachable!()
    }
}

impl<T> AsRef<T> for ThreadState<T> {
    fn as_ref(&self) -> &T {
        &self.record_or_replay
    }
}

impl<T> AsMut<T> for ThreadState<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.record_or_replay
    }
}

#[allow(dead_code)]
fn into_atflags(flags: OFlag) -> AtFlags {
    // NB: we're only interested with stat* with this fd.
    if flags.contains(OFlag::O_NOFOLLOW) {
        AtFlags::AT_SYMLINK_NOFOLLOW
    } else {
        AtFlags::empty()
    }
}

#[allow(dead_code)]
fn from_atflags(flags: AtFlags) -> OFlag {
    // NB: we're only interested with stat* with this fd.
    if flags.contains(AtFlags::AT_SYMLINK_NOFOLLOW) {
        OFlag::O_PATH | OFlag::O_NOFOLLOW
    } else {
        OFlag::O_PATH
    }
}

impl<T> ThreadState<T> {
    /// Create a fresh new thread state from nothing.  In practice this is only used for the thread
    /// state of the root thread of the container.
    pub fn new(pid: DetPid, cfg: &Config, record_or_replay: T) -> Self {
        detlog!(
            "USER RAND: seeding PRNG for root thread with seed {}",
            cfg.rng_seed()
        );
        detlog!(
            "CHAOSRAND: seeding chaos scheduler with seed {}",
            cfg.sched_seed()
        );
        ThreadState {
            dettid: pid,
            detpid: None, // Initialized later.
            mm_id: MmId::initial(pid),
            memory_metadata: Arc::new(Mutex::new(MemoryMetadata::new())),
            pedigree: Pedigree::new(), // Root thread.
            stats: ThreadStats::new(),
            file_metadata: Arc::new(Mutex::new(
                FileMetadata::new(pid).setup_stdio(pid.into(), pid),
            )),
            posix_timers: Arc::new(Mutex::new(PosixTimers::default())),
            clone_flags: None,
            pending_vfork: None,
            // For the root thread, we initialize from the seed in the config:
            prng: Pcg64Mcg::seed_from_u64(cfg.rng_seed()),
            chaos_prng: Pcg64Mcg::seed_from_u64(cfg.sched_seed()),
            thread_logical_time: DetTime::new(cfg),
            committed_clock_value: 0,
            end_of_timeslice: None, // Temporary/bogus.
            last_rcb_timer: None,
            record_or_replay,
            preemption_points: None,
            past_global_first_execve: false,
            interrupt_at: cfg.interrupts_for_thread(pid),
        }
    }

    /// Resolve a futex key from its opcode mode and virtual address.
    pub(crate) fn futex_id(&self, address: usize, is_private: bool) -> FutexID {
        if is_private {
            FutexID::private(self.mm_id, address)
        } else {
            self.memory_metadata
                .lock()
                .expect("memory metadata mutex poisoned")
                .futex_id(self.mm_id, address)
        }
    }

    /// Record an anonymous shared mapping.
    pub(crate) fn map_shared_anonymous(&self, start: usize, len: usize) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .map_anonymous(self.mm_id, start, len);
    }

    /// Record a file-backed shared mapping.
    pub(crate) fn map_shared_object(
        &self,
        start: usize,
        len: usize,
        object: SharedMemoryObjectId,
        object_offset: u64,
    ) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .map_object(start, len, object, object_offset);
    }

    /// Remove a range from the shared mapping model.
    pub(crate) fn unmap_memory(&self, start: usize, len: usize) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .unmap(start, len);
    }

    /// Move or resize a range in the shared mapping model.
    pub(crate) fn remap_memory(
        &self,
        old_start: usize,
        old_len: usize,
        new_start: usize,
        new_len: usize,
    ) {
        self.memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .remap(old_start, old_len, new_start, new_len);
    }

    /// Build a singleton resource request from the current thread.
    pub fn mk_request(&self, rid: ResourceID, perm: Permission) -> Resources {
        let mut resources = HashMap::new();
        resources.insert(rid, perm);
        Resources {
            tid: self.dettid,
            resources,
            poll_attempt: 0,
            fyi: String::new(),
        }
    }

    /// Generate the next random number using the thread-local chaos_seed.
    pub fn chaos_prng_next_u64(&mut self, msg: &str) -> u64 {
        let r = self.chaos_prng.next_u64();
        detlog!("[dtid {}] CHAOSRAND({}): u64 => {}", self.dettid, msg, r);
        r
    }

    /// get file metadata
    fn metadata(&self) -> MutexGuard<'_, FileMetadata> {
        self.file_metadata.lock().unwrap()
    }

    /// Add a new fd, with optional stat data, have side effects on other
    /// threads.
    ///
    /// If stat data is not available, then perform an extra stat ourselves to populate it.
    ///
    /// # Arguments
    ///
    /// * `fd` - file descriptor to add
    ///
    /// * `flags` - flags when creating `fd`
    ///
    /// * `ty` - fd type (regular file, socket, pipe, etc..)
    ///
    /// * `stat` - stat returned from fstat
    pub fn add_fd(
        &self,
        fd: RawFd,
        flags: OFlag,
        ty: FdType,
        stat: Option<DetStat>,
    ) -> Result<(), Errno> {
        self.metadata().add_fd(self.dettid, fd, flags, ty, stat)
    }

    /// Get a mutable reference of `DetFd` from a raw file descriptor, and
    /// run mutable function `f` on it (`&mut DetFd`).
    pub fn with_detfd<F, U>(&self, fd: RawFd, f: F) -> Result<U, Errno>
    where
        F: FnMut(&mut DetFd) -> U,
    {
        self.metadata().with_detfd(fd, f)
    }

    /// remove a rawfd
    pub fn remove_fd(&self, fd: RawFd) -> Option<OpenFileId> {
        self.metadata().remove_fd(fd)
    }

    /// dup raw fds.
    pub fn dup_fd(
        &mut self,
        oldfd: RawFd,
        newfd: RawFd,
        flags: OFlag,
    ) -> Result<Option<OpenFileId>, Errno> {
        self.metadata().dup_fd(oldfd, newfd, flags)
    }

    /// get thread prng, note this rng is deterministic and should not be used
    /// for crypto.
    pub fn thread_prng(&mut self) -> &mut Pcg64Mcg {
        &mut self.prng
    }

    /// Choose an amount of time (RCBs) for our next timeslice based on various settings.
    ///
    /// Effects:
    /// - Sets `end_of_timeslice` for the new timeslice.
    /// - Resets the statistics for the timeslice.
    ///
    /// Returns: an optional new priority.
    pub fn next_timeslice(&mut self, cfg: &Config) -> Option<Priority> {
        // If the preemption feature is disabled, this fizzles:
        if let Some(timeout_ns) = cfg.preemption_timeout {
            let current_ns = self.thread_logical_time.as_nanos();
            let mut result = None;

            // Preemption-point replay from recorded --chaos configuration.
            if let Some(thi) = &mut self.preemption_points {
                if self.stats.last_recorded_slice.is_none() {
                    // We have not tapped out the recording yet.
                    if let Some((end_time, prio)) = thi.next() {
                        debug!(
                            "[dtid {}] next timeslice (T{}), set by recording to {:?} (current {}), priority {}",
                            self.dettid,
                            self.stats.timeslice_count + 1,
                            end_time,
                            current_ns,
                            prio
                        );
                        if end_time <= current_ns {
                            panic!(
                                "Cannot set end of timeslice to {} for thread {}, when current thread logical time is already {}.",
                                end_time, self.dettid, current_ns
                            )
                        }
                        self.end_of_timeslice = Some(end_time);
                        result = Some(prio);
                    } else {
                        let max = LogicalTime::MAX;
                        let prio = thi.final_priority();
                        debug!(
                            "[dtid {}] next timeslice (T{}) final slice after recorded preemption points... setting end_of_timeslice to max {}, final priority {}",
                            self.dettid,
                            self.stats.timeslice_count + 1,
                            max,
                            prio
                        );
                        self.stats.last_recorded_slice = Some(self.stats.timeslice_count);
                        self.end_of_timeslice = Some(max);
                        result = Some(prio)
                    }
                } else {
                    tracing::warn!(
                        "[dtid {}] next timeslice: timer expired beyond the last recorded preemption.  Not handled yet.",
                        self.dettid
                    );
                    self.end_of_timeslice = Some(LogicalTime::MAX);
                    result = Some(thi.final_priority())
                }
            } else if !cfg.chaos {
                if cfg.replay_schedule_from.is_some() {
                    // This will be over written based on Branch count replayed IF needed.
                    debug!(
                        "[dtid {}] next timeslice (T{}), in replay mode setting timeslice to max (current time {})",
                        self.dettid,
                        self.stats.timeslice_count + 1,
                        current_ns
                    );
                    self.end_of_timeslice = Some(LogicalTime::MAX);
                } else {
                    // In non-chaos mode, we only care about preemption for breaking busy-waits,
                    // and we can safely reset the clock every time we get control back from the
                    // guest.  This is our preemption-of-last-resort:
                    self.end_of_timeslice =
                        Some(current_ns + Duration::from_nanos(u64::from(timeout_ns)));
                    debug!(
                        "[dtid {}] next timeslice (T{}), end of slice set to {} (current {})",
                        self.dettid,
                        self.stats.timeslice_count + 1,
                        self.end_of_timeslice.unwrap(),
                        current_ns,
                    );
                }
            } else {
                let target_timeout_rcbs = u64::from(timeout_ns) as f64 / NANOS_PER_RCB;
                let next_rcbs: u64 = if cfg.chaos {
                    // Average frequency of preemptions per nanosecond:
                    let lambda = 1.0 / target_timeout_rcbs;
                    let exp = Exp::new(lambda).unwrap();
                    // Add one to prevent generating a zero time slice:
                    let rcbs = 1 + exp.sample(&mut self.chaos_prng) as u64;
                    detlog!("[dtid {}] CHAOSRAND => next_rcbs = {}", self.dettid, rcbs);
                    rcbs
                } else {
                    target_timeout_rcbs as u64
                };
                assert!(next_rcbs > 0);
                self.last_rcb_timer = None;
                self.end_of_timeslice = Some(current_ns + rcbs_to_duration(next_rcbs));
                debug!(
                    "[dtid {}] next timeslice (T{}) chosen as {} rcbs, end of slice = {} (current {})",
                    self.dettid,
                    self.stats.timeslice_count + 1,
                    next_rcbs,
                    self.end_of_timeslice.unwrap(),
                    current_ns
                );
            }
            self.stats.reset_timeslice();
            result
        } else {
            None
        }
    }

    /// Are we within the execution of the (first) guest binary or any child processes called by it?
    /// Returns false if we are in the very beginning of execution, when the hermit container has
    /// forked our process, but we have not yet executed the guest binary.  There are few guarantees
    /// during this early initialization period, and Detcore should make no assumptions, nor
    /// guarantee determinism!
    pub fn guest_past_first_execve(&self) -> bool {
        self.past_global_first_execve
    }
}

/// Generate a new thread-local PRNG from the parent's PRNG state, mixing in the
/// new DetTid for some deterministic entropy. This ensures sequentially-spawned
/// threads get distinct PRNG states.
pub fn thread_rng_from_parent(msg: &str, parent: &Pcg64Mcg, child: DetTid) -> Pcg64Mcg {
    // Perform the default SeedableRng::from_seed procedure
    let mut seed = <Pcg64Mcg as SeedableRng>::Seed::default();
    // Generate a seed from the parent:
    parent.clone().fill_bytes(seed.as_mut());
    detlog!("RNG {} Generated new seed {:?}", msg, seed);
    // Perturb the seed by the tid
    let entropy = child.as_raw();
    seed[0] ^= entropy as u8;
    seed[1] ^= (entropy >> 8) as u8;
    seed[2] ^= (entropy >> 16) as u8;
    seed[3] ^= (entropy >> 24) as u8;
    detlog!(
        "RNG {} seeding child tid {}: {:?} from parent {:?}",
        msg,
        child,
        seed,
        parent
    );
    let mut rng = Pcg64Mcg::from_seed(seed);
    // Pcg64Mcg integrates flipped bits across the state quickly. Some PRNGs don't.
    // Defensively ensure flipped bits "propagate":
    rng.next_u64();
    rng.next_u64();
    rng.next_u64();
    rng.next_u64();
    rng
}
